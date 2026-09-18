//! Parsers for the git plumbing output the checkpoint store reads. Pure and
//! dependency-free so they can be unit-tested without a repository.

use super::{ChangeStatus, Checkpoint, CheckpointKind, DiffStat, FileStat};

/// Field separator used in our `git log --format` string (US, 0x1f). Chosen
/// because `sanitize_label` guarantees it cannot appear inside a label.
pub(super) const FS: char = '\u{1f}';

/// Parses `git log --format=%H<FS>%P<FS>%ct<FS>%B%x00`. Records are
/// NUL-terminated; commits without our trailers are skipped, since anything
/// in `refs/heads/magai` that magai did not write is not a checkpoint.
pub(super) fn parse_log(raw: &str) -> Vec<Checkpoint> {
    raw.split('\0')
        .filter(|rec| !rec.trim().is_empty())
        .filter_map(parse_record)
        .collect()
}

fn parse_record(rec: &str) -> Option<Checkpoint> {
    let mut fields = rec.trim_start_matches('\n').splitn(4, FS);
    let sha = fields.next()?.trim().to_string();
    let parents = fields.next()?.trim().to_string();
    let unix_time = fields.next()?.trim().parse::<i64>().ok()?;
    let body = fields.next()?;

    if sha.is_empty() {
        return None;
    }

    // A root commit has an empty %P; otherwise take the first parent.
    let parent = parents.split_whitespace().next().map(str::to_string);

    let id = trailer(body, "Magai-Checkpoint")?.parse::<u64>().ok()?;
    let kind = match trailer(body, "Magai-Kind").unwrap_or_default().as_str() {
        "baseline" => CheckpointKind::Baseline,
        "turn" => CheckpointKind::Turn,
        "undo" => CheckpointKind::Undo,
        "redo" => CheckpointKind::Redo,
        "restore" => CheckpointKind::Restore,
        _ => return None,
    };

    Some(Checkpoint {
        id,
        sha,
        parent,
        unix_time,
        kind,
        outcome: trailer(body, "Magai-Outcome"),
        label: trailer(body, "Magai-Label").unwrap_or_default(),
        model: trailer(body, "Magai-Model"),
        session: trailer(body, "Magai-Session").unwrap_or_default(),
        turn: trailer(body, "Magai-Turn").and_then(|t| t.parse().ok()),
    })
}

/// Reads a `Key: value` trailer out of a commit message body.
fn trailer(body: &str, key: &str) -> Option<String> {
    body.lines().find_map(|line| {
        let rest = line.strip_prefix(key)?.strip_prefix(':')?;
        let value = rest.trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

/// Parses `git diff-tree -r --name-status -z`: `status\0path\0` pairs.
pub(super) fn parse_name_status_z(raw: &str) -> Vec<(ChangeStatus, String)> {
    let mut out = Vec::new();
    let mut it = raw.split('\0').filter(|f| !f.is_empty());
    while let (Some(status), Some(path)) = (it.next(), it.next()) {
        let status = match status.chars().next() {
            Some('A') => ChangeStatus::Added,
            Some('M') => ChangeStatus::Modified,
            Some('D') => ChangeStatus::Deleted,
            Some('T') => ChangeStatus::TypeChanged,
            _ => ChangeStatus::Other,
        };
        out.push((status, path.to_string()));
    }
    out
}

/// Parses `git diff-tree -r --numstat -z`: `added\tremoved\tpath\0`, where a
/// binary file reports `-` for both counts.
pub(super) fn parse_numstat_z(raw: &str) -> Vec<(usize, usize, bool, String)> {
    raw.split('\0')
        .filter(|f| !f.is_empty())
        .filter_map(|rec| {
            let mut parts = rec.splitn(3, '\t');
            let added = parts.next()?;
            let removed = parts.next()?;
            let path = parts.next()?;
            let binary = added == "-" || removed == "-";
            Some((
                added.parse().unwrap_or(0),
                removed.parse().unwrap_or(0),
                binary,
                path.to_string(),
            ))
        })
        .collect()
}

/// Joins the two diff-tree passes into one stat, keyed by path. Paths present
/// only in the numstat pass still get a row, with an unknown status.
pub(super) fn merge_stat(
    name_status: Vec<(ChangeStatus, String)>,
    numstat: Vec<(usize, usize, bool, String)>,
) -> DiffStat {
    let mut files: Vec<FileStat> = Vec::with_capacity(numstat.len());
    let mut insertions = 0;
    let mut deletions = 0;

    for (added, removed, binary, path) in numstat {
        let status = name_status
            .iter()
            .find(|(_, p)| *p == path)
            .map(|(s, _)| *s)
            .unwrap_or(ChangeStatus::Other);
        insertions += added;
        deletions += removed;
        files.push(FileStat {
            status,
            path,
            added,
            removed,
            binary,
        });
    }

    DiffStat {
        files,
        insertions,
        deletions,
    }
}

/// Pulls the offending paths out of `git apply --check` stderr, so a refused
/// undo can name the files that collided. Relies on `LC_ALL=C` in the command
/// envelope to keep these messages stable.
pub(super) fn conflict_paths(stderr: &str) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for line in stderr.lines() {
        let line = line.trim();
        let candidate = line
            .strip_prefix("error: patch failed: ")
            .map(|rest| rest.rsplit_once(':').map(|(p, _)| p).unwrap_or(rest))
            .or_else(|| {
                line.strip_prefix("error: ")
                    .and_then(|rest| rest.split_once(':'))
                    .map(|(path, _)| path)
            });
        if let Some(path) = candidate {
            let path = path.trim();
            if !path.is_empty() && !paths.iter().any(|p| p == path) {
                paths.push(path.to_string());
            }
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(sha: &str, parents: &str, time: i64, body: &str) -> String {
        format!("{sha}{FS}{parents}{FS}{time}{FS}{body}\0")
    }

    fn body(id: u64, kind: &str, label: &str) -> String {
        format!(
            "magai: {kind} — {label}\n\nMagai-Version: 1\nMagai-Checkpoint: {id}\n\
             Magai-Kind: {kind}\nMagai-Label: {label}\nMagai-Session: abc\n"
        )
    }

    #[test]
    fn parse_log_reads_two_commits() {
        let raw = format!(
            "{}{}",
            record("aaa", "bbb", 100, &body(2, "turn", "second")),
            record("bbb", "", 50, &body(1, "baseline", "session start")),
        );
        let cps = parse_log(&raw);
        assert_eq!(cps.len(), 2);
        assert_eq!(cps[0].id, 2);
        assert_eq!(cps[0].sha, "aaa");
        assert_eq!(cps[0].parent.as_deref(), Some("bbb"));
        assert_eq!(cps[0].label, "second");
        assert_eq!(cps[0].kind, CheckpointKind::Turn);
        // A root commit has an empty %P.
        assert_eq!(cps[1].parent, None);
        assert_eq!(cps[1].kind, CheckpointKind::Baseline);
    }

    #[test]
    fn parse_log_skips_commits_without_trailers() {
        let raw = record("aaa", "", 1, "just a message\n");
        assert!(parse_log(&raw).is_empty());
    }

    #[test]
    fn parse_log_handles_empty_input() {
        assert!(parse_log("").is_empty());
        assert!(parse_log("\0\0").is_empty());
    }

    #[test]
    fn parse_log_keeps_labels_containing_colons_and_blank_lines() {
        let b = "magai: turn\n\nMagai-Checkpoint: 7\nMagai-Kind: turn\n\
                 Magai-Label: fix: the thing\nMagai-Turn: 3\n";
        let cps = parse_log(&record("aaa", "bbb", 9, b));
        assert_eq!(cps[0].label, "fix: the thing");
        assert_eq!(cps[0].turn, Some(3));
    }

    #[test]
    fn parse_numstat_handles_binary_and_odd_paths() {
        let raw = "1\t2\tsrc/a.rs\x00-\t-\tlogo.png\x003\t0\tdir/a file \"q\".txt\x00";
        let out = parse_numstat_z(raw);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], (1, 2, false, "src/a.rs".to_string()));
        assert_eq!(out[1], (0, 0, true, "logo.png".to_string()));
        // -z plus core.quotepath=false means no C-style unquoting is needed.
        assert_eq!(out[2].3, "dir/a file \"q\".txt");
    }

    #[test]
    fn parse_name_status_reads_pairs() {
        let out = parse_name_status_z("A\0new.rs\0M\0old.rs\0D\0gone.rs\0T\0link\0");
        assert_eq!(out.len(), 4);
        assert_eq!(out[0], (ChangeStatus::Added, "new.rs".to_string()));
        assert_eq!(out[2].0, ChangeStatus::Deleted);
        assert_eq!(out[3].0, ChangeStatus::TypeChanged);
    }

    #[test]
    fn merge_stat_totals_and_pairs_status() {
        let stat = merge_stat(
            parse_name_status_z("M\0a.rs\0A\0b.rs\0"),
            parse_numstat_z("1\t2\ta.rs\0-\t-\tb.rs\0"),
        );
        assert_eq!(stat.insertions, 1);
        assert_eq!(stat.deletions, 2);
        assert_eq!(stat.files[0].status, ChangeStatus::Modified);
        assert_eq!(stat.files[1].status, ChangeStatus::Added);
        assert!(stat.files[1].binary);
    }

    #[test]
    fn conflict_paths_parses_apply_errors() {
        let stderr = "error: patch failed: src/a.rs:12\n\
                      error: src/a.rs: patch does not apply\n\
                      error: src/b.rs: No such file or directory\n";
        assert_eq!(conflict_paths(stderr), vec!["src/a.rs", "src/b.rs"]);
    }

    #[test]
    fn conflict_paths_empty_when_no_errors() {
        assert!(conflict_paths("").is_empty());
    }
}
