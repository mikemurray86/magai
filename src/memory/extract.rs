use std::sync::Arc;

use rusqlite::Connection;
use serde_json::Value;
use uuid::Uuid;

use super::graph::{EdgeKind, NodeKind};
use super::MemoryDb;

pub struct ToolCallRecord {
    pub name: String,
    pub args_json: String,
    // Stored for v3 LLM-based fact extraction; not used in v1/v2 rule extraction.
    #[allow(dead_code)]
    pub result: String,
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

/// Fire-and-forget LLM-based fact extraction. Calls the Ollama generate
/// endpoint with `model`, parses the JSON triple array from the response, and
/// stores each triple as a `Fact` node. Runs in a spawned task; all errors are
/// silently swallowed so a bad model response never interrupts the agent.
pub async fn extract_facts_async(
    db: Arc<MemoryDb>,
    text: String,
    model: String,
    session_alias: String,
) {
    let prompt = format!(
        "Extract factual claims from the text below as a JSON array. \
         Each element must be an object with exactly three string fields: \
         \"subject\", \"predicate\", \"object\". \
         Only extract clear, objective facts — skip opinions, questions, and instructions. \
         Return [] if nothing is worth storing. \
         Respond with valid JSON only, no other text.\n\nText:\n{text}\n\nJSON:"
    );

    let client = reqwest::Client::new();
    let Ok(resp) = client
        .post("http://localhost:11434/api/generate")
        .json(&serde_json::json!({
            "model": model,
            "prompt": prompt,
            "stream": false
        }))
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
    else {
        return;
    };

    let Ok(body) = resp.json::<Value>().await else {
        return;
    };

    let response_text = match body["response"].as_str() {
        Some(s) => s.trim().to_owned(),
        None => return,
    };

    // Try to parse directly, then fall back to finding the outermost [...].
    let triples: Value = serde_json::from_str(&response_text).unwrap_or_else(|_| {
        let start = response_text.find('[').unwrap_or(0);
        let end = response_text.rfind(']').map(|i| i + 1).unwrap_or(0);
        if end > start {
            serde_json::from_str(&response_text[start..end]).unwrap_or(Value::Null)
        } else {
            Value::Null
        }
    });

    let Some(arr) = triples.as_array() else {
        return;
    };

    db.with(|conn| {
        let now = epoch_secs();
        // Keep session node fresh.
        upsert_node(conn, NodeKind::Session, &session_alias, now);

        for triple in arr {
            let subject = match triple["subject"].as_str().filter(|s| !s.is_empty()) {
                Some(s) => s,
                None => continue,
            };
            let predicate = match triple["predicate"].as_str().filter(|s| !s.is_empty()) {
                Some(s) => s,
                None => continue,
            };
            let object = match triple["object"].as_str().filter(|s| !s.is_empty()) {
                Some(s) => s,
                None => continue,
            };

            let fact_name = format!("{subject} {predicate} {object}");
            upsert_node(conn, NodeKind::Fact, &fact_name, now);

            // Link subject and object as concept tags so they appear in FTS search.
            let subj_id = upsert_node(conn, NodeKind::ConceptTag, subject, now);
            let obj_id = upsert_node(conn, NodeKind::ConceptTag, object, now);
            related_to(conn, &subj_id, &obj_id, now);
        }
    });
}
