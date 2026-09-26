use rusqlite::Connection;
use serde_json::Value;
use uuid::Uuid;

use super::graph::{EdgeKind, NodeKind};
use super::MemoryDb;

pub struct ToolCallRecord {
    pub name: String,
    pub args_json: String,
    pub result: String,
    pub elapsed_ms: u64,
}

/// Extracts entities from `records` and persists them to the graph.
///
/// Per-turn entities that co-occur (e.g. two files touched in the same turn)
/// get a RELATED_TO edge whose weight is incremented each time they appear
/// together, building up a co-occurrence signal over sessions.
pub fn process_turn(db: &MemoryDb, session_alias: &str, records: &[ToolCallRecord]) {
    if records.is_empty() {
        return;
    }
    db.with(|conn| {
        let now = epoch_secs();
        let session_id = upsert_node(conn, NodeKind::Session, session_alias, now);
        let turn_id = Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO nodes(id, kind, name, data, created_at, last_seen_at, access_count) \
             VALUES (?1, 'turn', ?2, '{}', ?3, ?3, 1)",
            rusqlite::params![&turn_id, &format!("{session_alias}/{now}"), now],
        )
        .ok();
        edge(conn, &turn_id, &session_id, EdgeKind::OccurredIn, now);

        let mut entity_ids: Vec<String> = Vec::new();

        for record in records {
            let args: Value = serde_json::from_str(&record.args_json).unwrap_or(Value::Null);
            let entity_id = match record.name.as_str() {
                "write_file" | "edit_file" => {
                    path_arg(&args).map(|p| (NodeKind::File, p.to_owned(), EdgeKind::Modifies))
                }
                "read_file" | "read_file_range" => {
                    path_arg(&args).map(|p| (NodeKind::File, p.to_owned(), EdgeKind::Reads))
                }
                "grep_search" => args
                    .get("query")
                    .and_then(Value::as_str)
                    .map(|q| (NodeKind::ConceptTag, q.to_owned(), EdgeKind::Mentions)),
                "shell_command" => args.get("command").and_then(Value::as_str).map(|cmd| {
                    let first_arg = args
                        .get("args")
                        .and_then(Value::as_array)
                        .and_then(|a| a.first())
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let concept = if first_arg.is_empty() {
                        cmd.to_owned()
                    } else {
                        format!("{cmd} {first_arg}")
                    };
                    (NodeKind::ConceptTag, concept, EdgeKind::Mentions)
                }),
                _ => None,
            };
            if let Some((kind, name, ek)) = entity_id {
                if !name.is_empty() {
                    let eid = upsert_node(conn, kind, &name, now);
                    edge(conn, &turn_id, &eid, ek, now);
                    entity_ids.push(eid);
                }
            }
        }

        // v2: co-occurrence RELATED_TO edges between entities that appear in
        // the same turn. Weight increments on each co-occurrence.
        for i in 0..entity_ids.len() {
            for j in (i + 1)..entity_ids.len() {
                related_to(conn, &entity_ids[i], &entity_ids[j], now);
            }
        }
    });
}

/// Explicitly persists a free-text note as a `Fact` node linked to the session.
/// Called by the `memory_save` tool so agents can proactively write memories.
pub fn save_note(db: &MemoryDb, text: &str, session_alias: &str) {
    db.with(|conn| {
        let now = epoch_secs();
        let session_id = upsert_node(conn, NodeKind::Session, session_alias, now);
        let fact_id = upsert_node(conn, NodeKind::Fact, text, now);
        edge(conn, &fact_id, &session_id, EdgeKind::OccurredIn, now);
    });
}

fn path_arg(args: &Value) -> Option<&str> {
    args.get("path").and_then(Value::as_str)
}

fn upsert_node(conn: &Connection, kind: NodeKind, name: &str, now: i64) -> String {
    let existing: rusqlite::Result<String> = conn.query_row(
        "SELECT id FROM nodes WHERE kind = ?1 AND name = ?2",
        rusqlite::params![kind.to_string(), name],
        |row| row.get(0),
    );
    if let Ok(id) = existing {
        conn.execute(
            "UPDATE nodes \
             SET last_seen_at = ?1, access_count = access_count + 1 \
             WHERE id = ?2",
            rusqlite::params![now, &id],
        )
        .ok();
        return id;
    }
    let id = Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO nodes(id, kind, name, data, created_at, last_seen_at, access_count) \
         VALUES (?1, ?2, ?3, '{}', ?4, ?4, 1)",
        rusqlite::params![&id, kind.to_string(), name, now],
    )
    .ok();
    conn.execute(
        "INSERT INTO nodes_fts(node_id, name) VALUES (?1, ?2)",
        rusqlite::params![&id, name],
    )
    .ok();
    id
}

fn edge(conn: &Connection, from_id: &str, to_id: &str, kind: EdgeKind, now: i64) {
    let id = Uuid::new_v4().to_string();
    conn.execute(
        "INSERT OR IGNORE INTO edges(id, from_id, to_id, kind, weight, created_at) \
         VALUES (?1, ?2, ?3, ?4, 1.0, ?5)",
        rusqlite::params![&id, from_id, to_id, kind.to_string(), now],
    )
    .ok();
}

/// Upserts a bidirectional RELATED_TO edge, incrementing weight on conflict.
fn related_to(conn: &Connection, a: &str, b: &str, now: i64) {
    for (from, to) in [(a, b), (b, a)] {
        let id = Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO edges(id, from_id, to_id, kind, weight, created_at) \
             VALUES (?1, ?2, ?3, 'related_to', 1.0, ?4) \
             ON CONFLICT(from_id, to_id, kind) DO UPDATE SET weight = weight + 1.0",
            rusqlite::params![&id, from, to, now],
        )
        .ok();
    }
}

fn epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// The built-in instructions for the fact extractor, used as its system
/// prompt unless `[memory.extractor]` sets `prompt`/`prompt_file`. The text
/// to classify is sent as the user message.
pub const DEFAULT_EXTRACTOR_PROMPT: &str = "Extract factual claims from the text the user sends \
     as a JSON array. Each element must be an object with exactly three string fields: \
     \"subject\", \"predicate\", \"object\". \
     Only extract clear, objective facts — skip opinions, questions, and instructions. \
     Return [] if nothing is worth storing. \
     Respond with valid JSON only, no other text.";

/// A `(subject, predicate, object)` fact pulled out of a model reply.
pub type Triple = (String, String, String);

/// Parses the extractor's reply into triples. Tolerates prose or code fences
/// around the array by falling back to the outermost `[...]`, and skips any
/// element missing a non-empty `subject`, `predicate` or `object`.
pub fn parse_triples(response: &str) -> Vec<Triple> {
    let response = response.trim();
    let parsed: Value = serde_json::from_str(response).unwrap_or_else(|_| {
        let start = response.find('[').unwrap_or(0);
        let end = response.rfind(']').map(|i| i + 1).unwrap_or(0);
        if end > start {
            serde_json::from_str(&response[start..end]).unwrap_or(Value::Null)
        } else {
            Value::Null
        }
    });
    let Some(arr) = parsed.as_array() else {
        return Vec::new();
    };
    let field = |t: &Value, k: &str| {
        t[k].as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    arr.iter()
        .filter_map(|t| {
            Some((
                field(t, "subject")?,
                field(t, "predicate")?,
                field(t, "object")?,
            ))
        })
        .collect()
}

/// Stores each triple as a `Fact` node, with its subject and object as
/// `ConceptTag`s joined by a RELATED_TO edge so they surface in FTS search.
pub fn store_facts(db: &MemoryDb, session_alias: &str, triples: &[Triple]) {
    if triples.is_empty() {
        return;
    }
    db.with(|conn| {
        let now = epoch_secs();
        // Keep session node fresh.
        upsert_node(conn, NodeKind::Session, session_alias, now);

        for (subject, predicate, object) in triples {
            upsert_node(
                conn,
                NodeKind::Fact,
                &format!("{subject} {predicate} {object}"),
                now,
            );
            let subj_id = upsert_node(conn, NodeKind::ConceptTag, subject, now);
            let obj_id = upsert_node(conn, NodeKind::ConceptTag, object, now);
            related_to(conn, &subj_id, &obj_id, now);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn triple(s: &str, p: &str, o: &str) -> Triple {
        (s.to_owned(), p.to_owned(), o.to_owned())
    }

    #[test]
    fn parse_triples_reads_a_bare_array() {
        let got =
            parse_triples(r#"[{"subject":"magai","predicate":"is written in","object":"Rust"}]"#);
        assert_eq!(got, vec![triple("magai", "is written in", "Rust")]);
    }

    #[test]
    fn parse_triples_digs_the_array_out_of_surrounding_prose() {
        let got = parse_triples(
            "Sure! ```json\n[{\"subject\":\"a\",\"predicate\":\"b\",\"object\":\"c\"}]\n```",
        );
        assert_eq!(got, vec![triple("a", "b", "c")]);
    }

    #[test]
    fn parse_triples_skips_incomplete_elements_and_junk() {
        let got = parse_triples(
            r#"[{"subject":"a","predicate":"","object":"c"}, {"subject":"x"}, 7,
                {"subject":" s ","predicate":"p","object":"o"}]"#,
        );
        assert_eq!(got, vec![triple("s", "p", "o")]);
        assert!(parse_triples("no json here").is_empty());
        assert!(parse_triples(r#"{"subject":"a"}"#).is_empty());
    }

    #[test]
    fn store_facts_writes_fact_and_concept_nodes() {
        let path = std::env::temp_dir().join(format!("magai-extract-test-{}.db", Uuid::new_v4()));
        let db = MemoryDb::open(&path).expect("open temp db");
        store_facts(&db, "local", &[triple("magai", "uses", "ratatui")]);

        let count = |kind: &str| -> i64 {
            db.with(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM nodes WHERE kind = ?1",
                    rusqlite::params![kind],
                    |row| row.get(0),
                )
            })
            .unwrap()
        };
        assert_eq!(count(&NodeKind::Fact.to_string()), 1);
        assert_eq!(count(&NodeKind::ConceptTag.to_string()), 2);
        std::fs::remove_file(&path).ok();
    }
}
