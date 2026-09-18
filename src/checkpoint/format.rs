//! Rendering for the checkpoint slash commands. Pure string formatting, kept
//! separate from the git plumbing so it can be tested without a repository.

use super::{ChangeStatus, Checkpoint, CheckpointKind, DiffStat};

/// Renders the `/checkpoints` table, newest first.
pub fn render_list(cps: &[Checkpoint], now: i64) -> String {
    if cps.is_empty() {
        return "no checkpoints yet".to_string();
    }
    let mut out = String::from("checkpoints (newest first):\n");
    for cp in cps {
        let kind = match cp.kind {
            CheckpointKind::Baseline => "baseline",
            CheckpointKind::Turn => "turn",
            CheckpointKind::Undo => "pre-undo",
            CheckpointKind::Redo => "pre-redo",
            CheckpointKind::Restore => "pre-restore",
        };
        let outcome = match cp.outcome.as_deref() {
            Some("cancelled") => " (cancelled)",
            Some("max_turns") => " (paused)",
            _ => "",
        };
        out.push_str(&format!(
            "  #{:<4} {:>9}  {:<11}{}  {}\n",
            cp.id,
            relative_time(cp.unix_time, now),
            kind,
            outcome,
            truncate_chars(&cp.label, 48),
        ));
    }
    out.push_str("\n  /diff <n> to see one, /restore <n> to reset the tree to it");
    out
}

/// Renders a per-file stat block, used by the confirmation card and by the
/// message printed after an undo or restore lands.
pub fn render_stat(stat: &DiffStat, max_files: usize) -> Vec<String> {
    if stat.files.is_empty() {
        return vec!["no file changes".to_string()];
    }
    let mut lines: Vec<String> = stat
        .files
        .iter()
        .take(max_files)
        .map(|f| {
            let mark = match f.status {
                ChangeStatus::Added => 'A',
                ChangeStatus::Modified => 'M',
                ChangeStatus::Deleted => 'D',
                ChangeStatus::TypeChanged => 'T',
                ChangeStatus::Other => '?',
            };
            let counts = if f.binary {
                "binary".to_string()
            } else {
                format!("+{} -{}", f.added, f.removed)
            };
            format!("  {mark} {:<44} {counts}", truncate_chars(&f.path, 44))
        })
        .collect();

    if stat.files.len() > max_files {
        lines.push(format!("  … and {} more", stat.files.len() - max_files));
    }
    lines.push(format!(
        "  {} file{}, +{} -{}",
        stat.files.len(),
        if stat.files.len() == 1 { "" } else { "s" },
        stat.insertions,
        stat.deletions
    ));
    lines
}

/// Coarse relative age; exact timestamps are not useful at a glance.
pub(super) fn relative_time(then: i64, now: i64) -> String {
    let secs = now.saturating_sub(then);
    if secs < 0 {
        return "just now".to_string();
    }
    match secs {
        0..=44 => "just now".to_string(),
        45..=5399 => format!("{}m ago", (secs + 30) / 60),
        5400..=86_399 => format!("{}h ago", (secs + 1800) / 3600),
        86_400..=172_799 => "yesterday".to_string(),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// Truncates on a char boundary, never mid-codepoint.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::FileStat;

    fn cp(id: u64, kind: CheckpointKind, label: &str, t: i64) -> Checkpoint {
        Checkpoint {
            id,
            sha: format!("sha{id}"),
            parent: None,
            unix_time: t,
            kind,
            outcome: None,
            label: label.to_string(),
            model: None,
            session: "s".to_string(),
            turn: None,
        }
    }

    #[test]
    fn relative_time_buckets() {
        assert_eq!(relative_time(1000, 1000), "just now");
        assert_eq!(relative_time(1000, 1030), "just now");
        assert_eq!(relative_time(0, 120), "2m ago");
        assert_eq!(relative_time(0, 7200), "2h ago");
        assert_eq!(relative_time(0, 90_000), "yesterday");
        assert_eq!(relative_time(0, 300_000), "3d ago");
        // A clock that jumped backwards must not underflow.
        assert_eq!(relative_time(2000, 1000), "just now");
    }

    #[test]
    fn render_list_empty() {
        assert_eq!(render_list(&[], 0), "no checkpoints yet");
    }

    #[test]
    fn render_list_shows_ids_and_labels() {
        let cps = vec![
            cp(2, CheckpointKind::Turn, "fix the parser", 985),
            cp(1, CheckpointKind::Baseline, "session start", 0),
        ];
        let out = render_list(&cps, 1000);
        assert!(out.contains("#2"));
        assert!(out.contains("fix the parser"));
        assert!(out.contains("baseline"));
        assert!(out.contains("just now"));
    }

    #[test]
    fn render_list_marks_cancelled_turns() {
        let mut c = cp(3, CheckpointKind::Turn, "half a thing", 0);
        c.outcome = Some("cancelled".to_string());
        assert!(render_list(&[c], 0).contains("(cancelled)"));
    }

    #[test]
    fn render_stat_totals_and_truncates() {
        let files: Vec<FileStat> = (0..5)
            .map(|i| FileStat {
                status: ChangeStatus::Modified,
                path: format!("src/f{i}.rs"),
                added: 2,
                removed: 1,
                binary: false,
            })
            .collect();
        let stat = DiffStat {
            files,
            insertions: 10,
            deletions: 5,
        };
        let lines = render_stat(&stat, 3);
        assert_eq!(lines.len(), 5); // 3 files + "and 2 more" + total
        assert!(lines[3].contains("and 2 more"));
        assert!(lines[4].contains("5 files, +10 -5"));
    }

    #[test]
    fn render_stat_singular_and_binary() {
        let stat = DiffStat {
            files: vec![FileStat {
                status: ChangeStatus::Added,
                path: "logo.png".to_string(),
                added: 0,
                removed: 0,
                binary: true,
            }],
            insertions: 0,
            deletions: 0,
        };
        let lines = render_stat(&stat, 10);
        assert!(lines[0].contains("binary"));
        assert!(lines[1].contains("1 file,"));
    }

    #[test]
    fn render_stat_empty() {
        assert_eq!(
            render_stat(&DiffStat::default(), 5),
            vec!["no file changes"]
        );
    }

    #[test]
    fn truncate_is_char_safe() {
        let s = "日本語のとても長いラベルです";
        let out = truncate_chars(s, 5);
        assert_eq!(out.chars().count(), 5);
        assert!(out.ends_with('…'));
        assert_eq!(truncate_chars("short", 10), "short");
    }
}
