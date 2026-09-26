//! Session/turn transcript persistence and quality-rating storage, for a
//! future fine-tuning export. Reuses the shared `MemoryDb` connection and
//! `sessions`/`turns`/`tool_calls`/`turn_ratings` tables from `schema.rs`.

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

/// The built-in instructions for the quality judge, used as its system
/// prompt unless `[quality.judge]` sets `prompt`/`prompt_file`. The turn is
/// sent as the user message, formatted by [`judge_input`].
pub const DEFAULT_JUDGE_PROMPT: &str = "Judge the quality of the AI coding-agent response \
     to a user request that the user sends you. Consider correctness, relevance, and \
     completeness. Respond with a single JSON object with exactly three fields: \
     \"verdict\" (one of \"good\", \"bad\", \"neutral\"), \"score\" (a number \
     between 0.0 and 1.0), and \"rationale\" (a short one-sentence explanation). \
     Respond with valid JSON only, no other text.";

/// The user message the judge rates.
pub fn judge_input(user_text: &str, assistant_text: &str) -> String {
    format!("User request:\n{user_text}\n\nAssistant response:\n{assistant_text}")
}

/// A judge's reply, parsed: `(verdict, score, rationale)`.
pub type Verdict = (String, Option<f64>, Option<String>);

/// Parses the judge's reply. Tolerates prose or code fences around the object
/// by falling back to the outermost `{...}`; `None` without a non-empty
/// `verdict`.
pub fn parse_verdict(response: &str) -> Option<Verdict> {
    let response = response.trim();
    let parsed: Value = serde_json::from_str(response).unwrap_or_else(|_| {
        let start = response.find('{').unwrap_or(0);
        let end = response.rfind('}').map(|i| i + 1).unwrap_or(0);
        if end > start {
            serde_json::from_str(&response[start..end]).unwrap_or(Value::Null)
        } else {
            Value::Null
        }
    });
    let verdict = parsed["verdict"].as_str().filter(|s| !s.is_empty())?;
    Some((
        verdict.to_owned(),
        parsed["score"].as_f64(),
        parsed["rationale"].as_str().map(str::to_owned),
    ))
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

    #[test]
    fn parse_verdict_reads_object_with_or_without_wrapping() {
        let v = parse_verdict(r#"{"verdict":"good","score":0.9,"rationale":"fine"}"#).unwrap();
        assert_eq!(v, ("good".to_string(), Some(0.9), Some("fine".to_string())));

        let v = parse_verdict("Here you go: {\"verdict\":\"bad\"} hope that helps").unwrap();
        assert_eq!(v, ("bad".to_string(), None, None));
    }

    #[test]
    fn parse_verdict_rejects_missing_verdict() {
        assert_eq!(parse_verdict(r#"{"score":0.5}"#), None);
        assert_eq!(parse_verdict(r#"{"verdict":""}"#), None);
        assert_eq!(parse_verdict("not json"), None);
    }
}
