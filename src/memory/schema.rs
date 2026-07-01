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
        ",
    )
}
