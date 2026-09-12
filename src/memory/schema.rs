use rusqlite::{Connection, Result};

pub fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS nodes (
            id           TEXT PRIMARY KEY,
            kind         TEXT NOT NULL,
            name         TEXT NOT NULL,
            data         TEXT NOT NULL DEFAULT '{}',
            created_at   INTEGER NOT NULL,
            last_seen_at INTEGER NOT NULL,
            access_count INTEGER NOT NULL DEFAULT 1
        );

        CREATE TABLE IF NOT EXISTS edges (
            id         TEXT PRIMARY KEY,
            from_id    TEXT NOT NULL,
            to_id      TEXT NOT NULL,
            kind       TEXT NOT NULL,
            weight     REAL NOT NULL DEFAULT 1.0,
            created_at INTEGER NOT NULL
        );

        -- Unique constraint on (from_id, to_id, kind) enables co-occurrence upserts
        -- that increment weight rather than inserting duplicates.
        CREATE UNIQUE INDEX IF NOT EXISTS idx_edges_unique    ON edges(from_id, to_id, kind);
        CREATE INDEX        IF NOT EXISTS idx_edges_from      ON edges(from_id);
        CREATE INDEX        IF NOT EXISTS idx_edges_to        ON edges(to_id);
        CREATE INDEX        IF NOT EXISTS idx_nodes_kind_name ON nodes(kind, name);

        CREATE VIRTUAL TABLE IF NOT EXISTS nodes_fts USING fts5(
            node_id UNINDEXED,
            name
        );

        -- Session/turn/quality tracking, for future fine-tuning export.
        CREATE TABLE IF NOT EXISTS sessions (
            id           TEXT PRIMARY KEY,
            model_alias  TEXT NOT NULL,
            started_at   INTEGER NOT NULL,
            ended_at     INTEGER,
            turn_count   INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS turns (
            id                  TEXT PRIMARY KEY,
            session_id          TEXT NOT NULL REFERENCES sessions(id),
            seq                 INTEGER NOT NULL,
            model_alias         TEXT NOT NULL,
            outcome             TEXT NOT NULL,
            user_text           TEXT NOT NULL,
            assistant_text      TEXT NOT NULL DEFAULT '',
            started_at          INTEGER NOT NULL,
            ended_at            INTEGER NOT NULL,
            tool_call_count     INTEGER NOT NULL DEFAULT 0,
            tool_failure_count  INTEGER NOT NULL DEFAULT 0,
            denied_count        INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_turns_session ON turns(session_id);

        CREATE TABLE IF NOT EXISTS tool_calls (
            id         TEXT PRIMARY KEY,
            turn_id    TEXT NOT NULL REFERENCES turns(id),
            seq        INTEGER NOT NULL,
            name       TEXT NOT NULL,
            args_json  TEXT NOT NULL,
            result     TEXT NOT NULL,
            elapsed_ms INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_tool_calls_turn ON tool_calls(turn_id);

        CREATE TABLE IF NOT EXISTS turn_ratings (
            id         TEXT PRIMARY KEY,
            turn_id    TEXT NOT NULL REFERENCES turns(id),
            source     TEXT NOT NULL,
            verdict    TEXT,
            score      REAL,
            rationale  TEXT,
            created_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_turn_ratings_turn ON turn_ratings(turn_id);
        ",
    )
}
