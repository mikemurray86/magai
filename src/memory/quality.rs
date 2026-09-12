//! Session/turn transcript persistence and quality-rating storage, for a
//! future fine-tuning export. Reuses the shared `MemoryDb` connection and
//! `sessions`/`turns`/`tool_calls`/`turn_ratings` tables from `schema.rs`.

use std::sync::Arc;

use rusqlite::params;
use serde_json::Value;
use uuid::Uuid;

use super::extract::ToolCallRecord;
use super::MemoryDb;

/// Starts a new session row (one per `run_agent` invocation) and returns its id.
pub fn start_session(db: &MemoryDb, model_alias: &str) -> String {
    let id = Uuid::new_v4().to_string();
    let now = epoch_secs();
    db.with(|conn| {
        conn.execute(
            "INSERT INTO sessions(id, model_alias, started_at, ended_at, turn_count) \
             VALUES (?1, ?2, ?3, NULL, 0)",
            params![&id, model_alias, now],
        )
        .ok();
    });
    id
}

/// Marks a session as finished.
pub fn end_session(db: &MemoryDb, session_id: &str) {
    let now = epoch_secs();
    db.with(|conn| {
        conn.execute(
            "UPDATE sessions SET ended_at = ?1 WHERE id = ?2",
            params![now, session_id],
        )
        .ok();
    });
}

/// Persists one finished turn plus its tool-call records. `tool_stats` is
/// `(total, failed, denied)`, sourced from the shared per-turn counter in
/// `approval.rs`. Returns the new turn id.
#[allow(clippy::too_many_arguments)]
pub fn record_turn(
    db: &MemoryDb,
    session_id: &str,
    seq: usize,
    model_alias: &str,
    outcome: &str,
    user_text: &str,
    assistant_text: &str,
    tool_records: &[ToolCallRecord],
    tool_stats: (usize, usize, usize),
    started_at: i64,
    ended_at: i64,
) -> String {
    let turn_id = Uuid::new_v4().to_string();
    let (_total, failed, denied) = tool_stats;
    db.with(|conn| {
        conn.execute(
            "INSERT INTO turns(id, session_id, seq, model_alias, outcome, user_text, \
             assistant_text, started_at, ended_at, tool_call_count, tool_failure_count, \
             denied_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                &turn_id,
                session_id,
                seq as i64,
                model_alias,
                outcome,
                user_text,
                assistant_text,
                started_at,
                ended_at,
                tool_records.len() as i64,
                failed as i64,
                denied as i64,
            ],
        )
        .ok();

        conn.execute(
            "UPDATE sessions SET turn_count = turn_count + 1 WHERE id = ?1",
            params![session_id],
        )
        .ok();

        for (i, record) in tool_records.iter().enumerate() {
            conn.execute(
                "INSERT INTO tool_calls(id, turn_id, seq, name, args_json, result, elapsed_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    Uuid::new_v4().to_string(),
                    &turn_id,
                    i as i64,
                    &record.name,
                    &record.args_json,
                    &record.result,
                    record.elapsed_ms as i64,
                ],
            )
            .ok();
        }
    });
    turn_id
}

/// Records a quality rating for a turn. `source` is `"implicit"`, `"user"`,
/// or `"judge"`; multiple ratings per turn (one per source) are expected.
pub fn record_rating(
    db: &MemoryDb,
    turn_id: &str,
    source: &str,
    verdict: Option<&str>,
    score: Option<f64>,
    rationale: Option<&str>,
) {
    let now = epoch_secs();
    db.with(|conn| {
        conn.execute(
            "INSERT INTO turn_ratings(id, turn_id, source, verdict, score, rationale, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                Uuid::new_v4().to_string(),
                turn_id,
                source,
                verdict,
                score,
                rationale,
                now
            ],
        )
        .ok();
    });
}

fn epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Fire-and-forget LLM-as-judge rating of a finished turn. Calls the Ollama
/// generate endpoint with `judge_model`, parses a JSON verdict object out of
/// the response, and stores it as a `turn_ratings` row with `source =
/// "judge"`. Runs in a spawned task; all errors are silently swallowed so a
/// bad model response never interrupts the agent.
pub async fn judge_turn_async(
    db: Arc<MemoryDb>,
    turn_id: String,
    judge_model: String,
    user_text: String,
    assistant_text: String,
) {
    let prompt = format!(
        "Judge the quality of the following AI coding-agent response to a user \
         request. Consider correctness, relevance, and completeness. \
         Respond with a single JSON object with exactly three fields: \
         \"verdict\" (one of \"good\", \"bad\", \"neutral\"), \"score\" (a number \
         between 0.0 and 1.0), and \"rationale\" (a short one-sentence \
         explanation). Respond with valid JSON only, no other text.\n\n\
         User request:\n{user_text}\n\nAssistant response:\n{assistant_text}\n\nJSON:"
    );

    let client = reqwest::Client::new();
    let Ok(resp) = client
        .post("http://localhost:11434/api/generate")
        .json(&serde_json::json!({
            "model": judge_model,
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

    // Try to parse directly, then fall back to finding the outermost {...}.
    let parsed: Value = serde_json::from_str(&response_text).unwrap_or_else(|_| {
        let start = response_text.find('{').unwrap_or(0);
        let end = response_text.rfind('}').map(|i| i + 1).unwrap_or(0);
        if end > start {
            serde_json::from_str(&response_text[start..end]).unwrap_or(Value::Null)
        } else {
            Value::Null
        }
    });

    let Some(verdict) = parsed["verdict"].as_str().filter(|s| !s.is_empty()) else {
        return;
    };
    let score = parsed["score"].as_f64();
    let rationale = parsed["rationale"].as_str();

    record_rating(&db, &turn_id, "judge", Some(verdict), score, rationale);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> MemoryDb {
        let path = std::env::temp_dir().join(format!("magai-quality-test-{}.db", Uuid::new_v4()));
        MemoryDb::open(&path).expect("open temp db")
    }

    #[test]
    fn start_and_end_session_round_trip() {
        let db = temp_db();
        let id = start_session(&db, "local");

        let model_alias: String = db
            .with(|conn| {
                conn.query_row(
                    "SELECT model_alias FROM sessions WHERE id = ?1",
                    params![&id],
                    |row| row.get(0),
                )
            })
            .expect("session row");
        assert_eq!(model_alias, "local");

        end_session(&db, &id);
        let ended_at: Option<i64> = db
            .with(|conn| {
                conn.query_row(
                    "SELECT ended_at FROM sessions WHERE id = ?1",
                    params![&id],
                    |row| row.get(0),
                )
            })
            .expect("session row");
        assert!(ended_at.is_some());
    }

    #[test]
    fn record_turn_inserts_turn_and_tool_calls() {
        let db = temp_db();
        let session_id = start_session(&db, "local");
        let tool_records = vec![ToolCallRecord {
            name: "read_file".to_string(),
            args_json: "{\"path\":\"a.rs\"}".to_string(),
            result: "contents".to_string(),
            elapsed_ms: 12,
        }];

        let turn_id = record_turn(
            &db,
            &session_id,
            1,
            "local",
            "done",
            "hello",
            "hi there",
            &tool_records,
            (1, 0, 0),
            100,
            105,
        );

        let (outcome, tool_call_count): (String, i64) = db
            .with(|conn| {
                conn.query_row(
                    "SELECT outcome, tool_call_count FROM turns WHERE id = ?1",
                    params![&turn_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
            })
            .expect("turn row");
        assert_eq!(outcome, "done");
        assert_eq!(tool_call_count, 1);

        let tool_name: String = db
            .with(|conn| {
                conn.query_row(
                    "SELECT name FROM tool_calls WHERE turn_id = ?1",
                    params![&turn_id],
                    |row| row.get(0),
                )
            })
            .expect("tool_call row");
        assert_eq!(tool_name, "read_file");

        let turn_count: i64 = db
            .with(|conn| {
                conn.query_row(
                    "SELECT turn_count FROM sessions WHERE id = ?1",
                    params![&session_id],
                    |row| row.get(0),
                )
            })
            .expect("session row");
        assert_eq!(turn_count, 1);
    }

    #[test]
    fn record_rating_persists_each_source() {
        let db = temp_db();
        let session_id = start_session(&db, "local");
        let turn_id = record_turn(
            &db,
            &session_id,
            1,
            "local",
            "done",
            "hi",
            "hello",
            &[],
            (0, 0, 0),
            0,
            1,
        );

        record_rating(&db, &turn_id, "user", Some("good"), None, Some("nice"));
        record_rating(
            &db,
            &turn_id,
            "judge",
            Some("bad"),
            Some(0.2),
            Some("wrong file"),
        );

        let count: i64 = db
            .with(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM turn_ratings WHERE turn_id = ?1",
                    params![&turn_id],
                    |row| row.get(0),
                )
            })
            .expect("count");
        assert_eq!(count, 2);
    }
}
