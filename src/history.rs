//! Persistent prompt history for Up/Down recall in the input box. One global
//! file (`$XDG_DATA_HOME/magai/history.jsonl`) shared by every project and
//! session, so a restart — or another magai running alongside — sees the same
//! list. Each line is one JSON string, which keeps multi-line messages intact.
//!
//! Writes are appends, so concurrent instances interleave rather than clobber
//! each other; the file is only rewritten (via temp file + rename) when it has
//! grown to twice the cap.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Entries kept in memory and, after compaction, on disk.
const MAX_ENTRIES: usize = 1000;

pub struct History {
    path: Option<PathBuf>,
    entries: Vec<String>,
    /// Lines currently in the file, including ones past the cap.
    lines_on_disk: usize,
}

impl History {
    /// `$XDG_DATA_HOME/magai/history.jsonl`, mirroring `memory::default_db_path`.
    pub fn default_path() -> PathBuf {
        let base = std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .unwrap_or_else(|_| PathBuf::from("/tmp"));
        base.join("magai").join("history.jsonl")
    }

    /// Loads the file at `path`; a missing file is an empty history. An
    /// unreadable one still yields a working (in-memory) history plus a
    /// warning for the TUI.
    pub fn load(path: PathBuf) -> (Self, Option<String>) {
        match fs::read_to_string(&path) {
            Ok(content) => {
                let lines_on_disk = content.lines().count();
                let entries = parse(&content, MAX_ENTRIES);
                (
                    Self {
                        path: Some(path),
                        entries,
                        lines_on_disk,
                    },
                    None,
                )
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (
                Self {
                    path: Some(path),
                    entries: Vec::new(),
                    lines_on_disk: 0,
                },
                None,
            ),
            Err(e) => (
                Self::in_memory(),
                Some(format!(
                    "could not read prompt history {}: {e}; history won't be saved this session",
                    path.display()
                )),
            ),
        }
    }

    pub fn in_memory() -> Self {
        Self {
            path: None,
            entries: Vec::new(),
            lines_on_disk: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, idx: usize) -> Option<&str> {
        self.entries.get(idx).map(String::as_str)
    }

    /// Records `entry` unless it repeats the most recent one. The in-memory
    /// list is always updated; a disk error is returned for the caller to show.
    pub fn push(&mut self, entry: &str) -> Result<(), String> {
        if entry.trim().is_empty() || self.entries.last().is_some_and(|last| last == entry) {
            return Ok(());
        }
        self.entries.push(entry.to_string());
        if self.entries.len() > MAX_ENTRIES {
            let excess = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(..excess);
        }

        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        self.append(&path, entry)
            .map_err(|e| format!("could not save prompt history {}: {e}", path.display()))?;
        self.lines_on_disk += 1;
        if self.lines_on_disk >= MAX_ENTRIES * 2 {
            self.compact(&path)
                .map_err(|e| format!("could not compact prompt history {}: {e}", path.display()))?;
        }
        Ok(())
    }

    fn append(&self, path: &Path, entry: &str) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        // Prompts can contain pasted secrets; keep the file private.
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let mut file = opts.open(path)?;
        // One write per entry, so concurrent appenders don't split a line.
        file.write_all(format!("{}\n", encode(entry)).as_bytes())
    }

    /// Rewrites the file with just the newest `MAX_ENTRIES`, re-reading it
    /// first so entries appended by another running magai are kept.
    fn compact(&mut self, path: &Path) -> std::io::Result<()> {
        let current = parse(&fs::read_to_string(path)?, MAX_ENTRIES);
        let body: String = current.iter().map(|e| encode(e) + "\n").collect();
        let tmp = path.with_extension("jsonl.tmp");
        fs::write(&tmp, body)?;
        #[cfg(unix)]
        fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        fs::rename(&tmp, path)?;
        self.lines_on_disk = current.len();
        Ok(())
    }
}

fn encode(entry: &str) -> String {
    serde_json::to_string(entry).expect("serializing a str cannot fail")
}

/// The newest `cap` entries from a history file, oldest first. Lines that
/// aren't a JSON string (a torn write, hand edits) are skipped, as are
/// consecutive duplicates — two instances may record the same prompt.
fn parse(content: &str, cap: usize) -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    for line in content.lines() {
        let Ok(entry) = serde_json::from_str::<String>(line) else {
            continue;
        };
        if entries.last() != Some(&entry) {
            entries.push(entry);
        }
    }
    let excess = entries.len().saturating_sub(cap);
    entries.drain(..excess);
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("magai-history-{tag}-{}", uuid::Uuid::new_v4()))
            .join("history.jsonl")
    }

    #[test]
    fn parse_skips_garbage_and_consecutive_duplicates() {
        let content = "\"a\"\nnot json\n\"b\"\n\"b\"\n\"multi\\nline\"\n\"a\"\n";
        assert_eq!(parse(content, 10), ["a", "b", "multi\nline", "a"]);
    }

    #[test]
    fn parse_keeps_newest_when_over_cap() {
        let content = "\"1\"\n\"2\"\n\"3\"\n";
        assert_eq!(parse(content, 2), ["2", "3"]);
    }

    #[test]
    fn survives_reload() {
        let path = temp_path("reload");
        let (mut h, warn) = History::load(path.clone());
        assert!(warn.is_none());
        h.push("first").unwrap();
        h.push("second\nwith newline").unwrap();
        h.push("second\nwith newline").unwrap();
        h.push("   ").unwrap();

        let (h2, warn) = History::load(path.clone());
        assert!(warn.is_none());
        assert_eq!(h2.len(), 2);
        assert_eq!(h2.get(0), Some("first"));
        assert_eq!(h2.get(1), Some("second\nwith newline"));
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn compacts_to_cap() {
        let path = temp_path("compact");
        let (mut h, _) = History::load(path.clone());
        for i in 0..MAX_ENTRIES * 2 {
            h.push(&i.to_string()).unwrap();
        }
        let on_disk = fs::read_to_string(&path).unwrap().lines().count();
        assert_eq!(on_disk, MAX_ENTRIES);
        assert_eq!(h.len(), MAX_ENTRIES);
        assert_eq!(
            h.get(MAX_ENTRIES - 1),
            Some((MAX_ENTRIES * 2 - 1).to_string().as_str())
        );
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
