use super::MemoryDb;

pub struct MemorySnippet {
    pub kind: String,
    pub name: String,
}

/// FTS5 search with recency weighting: BM25 relevance score is multiplied by
/// `(1 + 1/(1 + age_days))` so nodes accessed within the last day receive up
/// to 2x the base relevance, decaying toward 1x as they age.
pub fn query_relevant(db: &MemoryDb, text: &str, limit: usize) -> Vec<MemorySnippet> {
    let q = build_fts_query(text);
    if q.is_empty() {
        return Vec::new();
    }
    db.with(|conn| {
        let Ok(mut stmt) = conn.prepare(
            "SELECT n.kind, n.name,
                    (-nodes_fts.rank) * (1.0 + 1.0 /
                        (1.0 + CAST(strftime('%s','now') - n.last_seen_at AS REAL) / 86400.0)
                    ) AS score
             FROM nodes_fts
             JOIN nodes n ON n.id = nodes_fts.node_id
             WHERE nodes_fts MATCH ?1
             ORDER BY score DESC
             LIMIT ?2",
        ) else {
            return Vec::new();
        };
        stmt.query_map(rusqlite::params![q, limit as i64], |row| {
            Ok(MemorySnippet {
                kind: row.get(0)?,
                name: row.get(1)?,
            })
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    })
}

/// FTS5 search + 1-hop RELATED_TO neighbors; used by the memory_query tool.
pub fn query_with_neighbors(db: &MemoryDb, text: &str, limit: usize) -> Vec<String> {
    let snippets = query_relevant(db, text, limit);
    if snippets.is_empty() {
        return Vec::new();
    }

    let match_ids: Vec<String> = db.with(|conn| {
        let q = build_fts_query(text);
        let Ok(mut stmt) = conn.prepare(
            "SELECT nodes_fts.node_id
             FROM nodes_fts
             WHERE nodes_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        ) else {
            return Vec::new();
        };
        stmt.query_map(rusqlite::params![q, limit as i64], |row| row.get(0))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    });

    let mut lines: Vec<String> = snippets
        .iter()
        .map(|s| format!("[{}] {}", s.kind, s.name))
        .collect();

    if !match_ids.is_empty() {
        let neighbors: Vec<(String, String, f64)> = db.with(|conn| {
            let placeholders: String = match_ids
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", i + 1))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT n.kind, n.name, e.weight
                 FROM edges e
                 JOIN nodes n ON n.id = e.to_id
                 WHERE e.from_id IN ({placeholders})
                   AND e.kind = 'related_to'
                 ORDER BY e.weight DESC
                 LIMIT 10"
            );
            let Ok(mut stmt) = conn.prepare(&sql) else {
                return Vec::new();
            };
            let params: Vec<&dyn rusqlite::ToSql> = match_ids
                .iter()
                .map(|id| id as &dyn rusqlite::ToSql)
                .collect();
            stmt.query_map(params.as_slice(), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, f64>(2)?,
                ))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
        });

        if !neighbors.is_empty() {
            lines.push(String::new());
            lines.push("Related:".to_string());
            for (kind, name, weight) in neighbors {
                lines.push(format!("  [{kind}] {name}  (co-occurrence: {weight:.0})"));
            }
        }
    }

    lines
}

/// Returns the most recently accessed nodes, formatted for display.
/// Used by `/memory` with no query argument.
pub fn recent_nodes(db: &MemoryDb, limit: usize) -> Vec<String> {
    db.with(|conn| {
        let Ok(mut stmt) = conn.prepare(
            "SELECT kind, name FROM nodes
             WHERE kind NOT IN ('turn', 'session')
             ORDER BY last_seen_at DESC
             LIMIT ?1",
        ) else {
            return Vec::new();
        };
        stmt.query_map(rusqlite::params![limit as i64], |row| {
            Ok(format!(
                "[{}] {}",
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?
            ))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    })
}

/// Wipes all memory graph data.
pub fn clear_all(db: &MemoryDb) -> rusqlite::Result<()> {
    db.with(|conn| {
        conn.execute_batch(
            "DELETE FROM nodes_fts; DELETE FROM edges; DELETE FROM nodes;",
        )
    })
}

fn build_fts_query(text: &str) -> String {
    let tokens: Vec<String> = text
        .split_whitespace()
        .filter(|w| {
            w.len() >= 3 && w.chars().all(|c| c.is_alphanumeric() || "._/-".contains(c))
        })
        .take(8)
        .map(|w| format!("\"{}\"", w.replace('"', "")))
        .collect();
    tokens.join(" OR ")
}
