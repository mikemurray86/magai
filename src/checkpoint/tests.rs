use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

// ── pure helpers ─────────────────────────────────────────────────────────────

#[test]
fn fnv1a_is_stable_and_distinguishes() {
    // Pinned literal: if this ever changes, every user's checkpoints orphan.
    assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
    assert_ne!(fnv1a64(b"a"), fnv1a64(b"b"));
}

#[test]
fn slug_is_deterministic_and_path_specific() {
    let a = slug_for(Path::new("/home/u/proj"));
    let b = slug_for(Path::new("/home/u/proj"));
    let c = slug_for(Path::new("/srv/other/proj"));
    assert_eq!(a, b);
    assert_ne!(
        a, c,
        "same basename in a different place must not share a store"
    );
    assert!(a.starts_with("proj-"));
}

#[test]
fn slug_sanitizes_awkward_names() {
    let s = slug_for(Path::new("/tmp/my project (v2)!"));
    assert!(s.starts_with("my-project--v2"), "got {s}");
    assert!(!s.contains(' ') && !s.contains('/') && !s.contains('!'));
    // A unicode basename still yields a usable directory name.
    assert!(!slug_for(Path::new("/tmp/日本語")).is_empty());
}

#[test]
fn project_root_finds_git_dir_or_file() {
    let t = temp("root");
    let nested = t.0.join("a/b/c");
    std::fs::create_dir_all(&nested).unwrap();
    // No .git anywhere: falls back to the starting directory.
    assert_eq!(project_root(&nested), nested);

    // .git as a directory.
    std::fs::create_dir_all(t.0.join("a/.git")).unwrap();
    assert_eq!(project_root(&nested), t.0.join("a"));

    // .git as a *file*, which is how linked worktrees and submodules look.
    std::fs::remove_dir_all(t.0.join("a/.git")).unwrap();
    std::fs::write(t.0.join("a/.git"), "gitdir: /elsewhere\n").unwrap();
    assert_eq!(project_root(&nested), t.0.join("a"));
}

#[test]
fn sanitize_label_flattens_and_bounds() {
    assert_eq!(sanitize_label("a\nb\r\nc"), "a b c");
    assert_eq!(sanitize_label("  lots   of   space  "), "lots of space");
    // The separators our log format relies on must never survive.
    let nasty = sanitize_label("a\u{1f}b\0c");
    assert!(!nasty.contains('\u{1f}') && !nasty.contains('\0'));

    let long = "x".repeat(500);
    let out = sanitize_label(&long);
    assert_eq!(out.chars().count(), 120);
    assert!(out.ends_with('…'));
}

#[test]
fn sanitize_label_truncates_on_char_boundary() {
    let out = sanitize_label(&"日".repeat(500));
    assert_eq!(out.chars().count(), 120);
}

#[test]
fn build_message_round_trips_through_parse_log() {
    let meta = SnapshotMeta::turn("fix: the retry backoff", "opus", 7, "cancelled");
    let msg = build_message(&meta, 42, "sess-1");
    let record = format!("abc123\u{1f}def456\u{1f}1700000000\u{1f}{msg}\0");
    let cps = parse::parse_log(&record);
    assert_eq!(cps.len(), 1);
    let cp = &cps[0];
    assert_eq!(cp.id, 42);
    assert_eq!(cp.kind, CheckpointKind::Turn);
    assert_eq!(cp.label, "fix: the retry backoff");
    assert_eq!(cp.outcome.as_deref(), Some("cancelled"));
    assert_eq!(cp.model.as_deref(), Some("opus"));
    assert_eq!(cp.turn, Some(7));
    assert_eq!(cp.session, "sess-1");
}

#[test]
fn exclude_file_composition() {
    let out = exclude_file(true, &["big-fixtures/".to_string()], Some("local-only\n"));
    assert!(out.contains("/.git/"));
    assert!(out.contains("target/"));
    assert!(out.contains("node_modules/"));
    assert!(out.contains("big-fixtures/"));
    assert!(out.contains("local-only"));

    // Without the defaults, .git must still be excluded.
    let bare = exclude_file(false, &[], None);
    assert!(bare.contains("/.git/"));
    assert!(!bare.contains("node_modules/"));
}

// ── integration ──────────────────────────────────────────────────────────────

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// Unique per test so the suite stays parallel-safe.
fn temp(tag: &str) -> TempDir {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("magai-test-ckpt-{tag}-{}-{n}", std::process::id()));
    std::fs::remove_dir_all(&p).ok();
    std::fs::create_dir_all(&p).unwrap();
    TempDir(p)
}

fn git_available() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

#[test]
fn shadow_roundtrip() {
    if !git_available() {
        return; // skip rather than fail where git is absent
    }
    let t = temp("roundtrip");
    let root = t.0.join("proj");
    let store = t.0.join("store");
    std::fs::create_dir_all(&root).unwrap();

    std::fs::write(root.join(".gitignore"), "build/\n").unwrap();
    std::fs::write(root.join("a.txt"), "hello\n").unwrap();
    std::fs::write(root.join("c.txt"), "keep\n").unwrap();
    std::fs::create_dir_all(root.join("build")).unwrap();
    std::fs::write(root.join("build/junk.o"), "junk\n").unwrap();
    // A .git directory in the project must never be snapshotted.
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

    let cfg = CheckpointsConfig::default();
    let s = CheckpointStore::open_at(&root, &store, &cfg, "sess").unwrap();
    // Opening twice must be idempotent.
    let s = {
        drop(s);
        CheckpointStore::open_at(&root, &store, &cfg, "sess").unwrap()
    };
    assert!(s.git_dir.join("info/attributes").exists());
    assert!(s.git_dir.join("info/exclude").exists());
    // git init must not have created a repository inside the project.
    assert!(!root.join(".git/objects").exists());

    // 1. baseline, then the empty-commit guard
    let base = match s.snapshot(&SnapshotMeta::baseline()).unwrap() {
        SnapshotOutcome::Created(cp) => *cp,
        SnapshotOutcome::Unchanged => panic!("baseline should commit"),
    };
    assert_eq!(base.id, 1);
    assert!(matches!(
        s.snapshot(&SnapshotMeta::baseline()).unwrap(),
        SnapshotOutcome::Unchanged
    ));

    // 2. .gitignore and .git/ are both respected
    let tracked = s.run(&["ls-files"]).unwrap();
    assert!(tracked.contains("a.txt"));
    assert!(
        !tracked.contains("junk.o"),
        "gitignored file was snapshotted"
    );
    assert!(!tracked.contains(".git/"), ".git was snapshotted");

    // 3. a turn: modify one file, create another, create a binary
    std::fs::write(root.join("a.txt"), "hello\nfrom the agent\n").unwrap();
    std::fs::write(root.join("b.txt"), "brand new\n").unwrap();
    std::fs::write(root.join("d.bin"), [b'x', 0, 1, 2, b'y']).unwrap();
    let turn = match s
        .snapshot(&SnapshotMeta::turn("do a thing", "m", 1, "done"))
        .unwrap()
    {
        SnapshotOutcome::Created(cp) => *cp,
        SnapshotOutcome::Unchanged => panic!("turn should commit"),
    };
    assert_eq!(turn.id, 2);
    let stat = s.stat_of(&turn).unwrap();
    assert_eq!(stat.files.len(), 3);
    assert!(stat.files.iter().any(|f| f.path == "d.bin" && f.binary));

    // 4. the user edits an unrelated file while the turn's changes stand
    std::fs::write(root.join("c.txt"), "keep\nuser edit\n").unwrap();

    // 5. surgical undo
    let plan = s.plan(CheckpointAction::Undo, None).unwrap();
    assert!(plan.blocked.is_none(), "clean undo was blocked: {plan:?}");
    assert_eq!(plan.target.sha, turn.sha);
    s.apply(&plan).unwrap();
    assert_eq!(read(&root.join("a.txt")), "hello\n");
    assert!(!root.join("b.txt").exists(), "created file must be deleted");
    assert!(!root.join("d.bin").exists());
    assert_eq!(
        read(&root.join("c.txt")),
        "keep\nuser edit\n",
        "the user's own concurrent edit must survive an undo"
    );

    // 6. redo puts it all back, binary included
    let plan = s.plan(CheckpointAction::Redo, None).unwrap();
    s.apply(&plan).unwrap();
    assert_eq!(read(&root.join("a.txt")), "hello\nfrom the agent\n");
    assert_eq!(
        std::fs::read(root.join("d.bin")).unwrap(),
        vec![b'x', 0, 1, 2, b'y'],
        "binary content must round-trip"
    );

    // 7. a genuine collision refuses, atomically
    std::fs::write(root.join("a.txt"), "TOTALLY DIFFERENT\ncontent\n").unwrap();
    let plan = s.plan(CheckpointAction::Undo, None).unwrap();
    assert!(plan.blocked.is_some(), "colliding undo should be blocked");
    assert!(matches!(
        s.apply(&plan),
        Err(CheckpointError::Unsupported(_))
    ));
    assert_eq!(
        read(&root.join("a.txt")),
        "TOTALLY DIFFERENT\ncontent\n",
        "a refused undo must write nothing"
    );

    // 8. full restore to the baseline
    let plan = s.plan(CheckpointAction::Restore(base.id), None).unwrap();
    s.apply(&plan).unwrap();
    assert_eq!(read(&root.join("a.txt")), "hello\n");
    assert_eq!(read(&root.join("c.txt")), "keep\n");
    assert!(!root.join("b.txt").exists());
    assert!(
        root.join("build/junk.o").exists(),
        "clean -fd must not remove gitignored files"
    );

    // 9. the log stays append-only, so history survives a restore
    let all = s.list(100).unwrap();
    assert!(all.len() >= 4, "got {} checkpoints", all.len());
    assert!(all.iter().any(|c| c.sha == turn.sha));
}

#[test]
fn prune_keeps_newest_and_preserves_ids() {
    if !git_available() {
        return;
    }
    let t = temp("prune");
    let root = t.0.join("proj");
    let store = t.0.join("store");
    std::fs::create_dir_all(&root).unwrap();

    let cfg = CheckpointsConfig {
        keep: 2,
        ..Default::default()
    };
    let s = CheckpointStore::open_at(&root, &store, &cfg, "sess").unwrap();
    for i in 0..5 {
        std::fs::write(root.join("f.txt"), format!("v{i}\n")).unwrap();
        s.snapshot(&SnapshotMeta::turn("t", "m", i, "done"))
            .unwrap();
    }
    let before = s.list(100).unwrap();
    assert_eq!(before.len(), 5);

    assert_eq!(s.prune().unwrap(), 3);
    let after = s.list(100).unwrap();
    assert_eq!(after.len(), 2);
    // Ids are monotonic and stable, so a written-down /restore 5 still works.
    assert_eq!(after[0].id, before[0].id);
    assert_eq!(after[1].id, before[1].id);
    assert_eq!(after[0].unix_time, before[0].unix_time);
    // The survivors are still usable.
    assert!(s.stat_of(&after[0]).is_ok());
}

#[test]
fn store_inside_worktree_is_refused() {
    let t = temp("selfref");
    let root = t.0.join("proj");
    std::fs::create_dir_all(&root).unwrap();
    let inside = root.join("store");
    let err = CheckpointStore::open_at(&root, &inside, &CheckpointsConfig::default(), "s");
    assert!(matches!(err, Err(CheckpointError::Unsupported(_))));
}

/// The whole point of the shadow repository: a full session's worth of
/// snapshots, undo and restore must leave the user's own repository bit-for-bit
/// unchanged.
#[test]
fn users_repository_is_never_touched() {
    if !git_available() {
        return;
    }
    let t = temp("isolation");
    let root = t.0.join("proj");
    let store = t.0.join("store");
    std::fs::create_dir_all(&root).unwrap();

    // A real repository, with a real commit and a deliberately dirty tree.
    let git = |args: &[&str]| -> String {
        let out = Command::new("git")
            .current_dir(&root)
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    git(&["init", "-q", "."]);
    git(&["config", "user.name", "T"]);
    git(&["config", "user.email", "t@e"]);
    std::fs::write(root.join("a.txt"), "one\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "initial"]);
    std::fs::write(root.join("dirty.txt"), "uncommitted\n").unwrap();

    let before_head = git(&["rev-parse", "HEAD"]);
    let before_status = git(&["status", "--porcelain"]);
    let before_stash = git(&["stash", "list"]);
    let before_objects = git(&["count-objects", "-v"]);
    let before_reflog = git(&["reflog"]);
    let before_branches = git(&["branch", "-a"]);
    // Sampled last, and re-checked first: `git status` itself refreshes the
    // index stat cache, so any git command would move this timestamp.
    let index_mtime = || {
        std::fs::metadata(root.join(".git/index"))
            .and_then(|m| m.modified())
            .ok()
    };
    let before_index = index_mtime();

    // A session: baseline, two turns, an undo and a restore.
    let cfg = CheckpointsConfig::default();
    let s = CheckpointStore::open_at(&root, &store, &cfg, "sess").unwrap();
    let base = match s.snapshot(&SnapshotMeta::baseline()).unwrap() {
        SnapshotOutcome::Created(cp) => cp.id,
        SnapshotOutcome::Unchanged => panic!("baseline should commit"),
    };
    std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
    s.snapshot(&SnapshotMeta::turn("t1", "m", 1, "done"))
        .unwrap();
    std::fs::write(root.join("b.txt"), "new\n").unwrap();
    s.snapshot(&SnapshotMeta::turn("t2", "m", 2, "done"))
        .unwrap();

    let plan = s.plan(CheckpointAction::Undo, None).unwrap();
    s.apply(&plan).unwrap();
    let plan = s.plan(CheckpointAction::Restore(base), None).unwrap();
    s.apply(&plan).unwrap();

    // Checked before any other git command, for the reason noted above.
    assert_eq!(before_index, index_mtime(), "the index was rewritten");

    // Every observable property of the user's repository must be unchanged.
    assert_eq!(git(&["rev-parse", "HEAD"]), before_head, "HEAD moved");
    assert_eq!(
        git(&["status", "--porcelain"]),
        before_status,
        "working tree status changed"
    );
    assert_eq!(
        git(&["stash", "list"]),
        before_stash,
        "something was stashed"
    );
    assert_eq!(
        git(&["count-objects", "-v"]),
        before_objects,
        "objects were written into the user's repo"
    );
    assert_eq!(git(&["reflog"]), before_reflog, "the reflog moved");
    assert_eq!(git(&["branch", "-a"]), before_branches, "branches changed");

    // ...while the user's own uncommitted file still exists, untouched.
    assert_eq!(read(&root.join("dirty.txt")), "uncommitted\n");
}

/// Checkpointing must work where there is no git repository at all — the case
/// the old implementation refused outright.
#[test]
fn works_in_a_non_git_project() {
    if !git_available() {
        return;
    }
    let t = temp("nongit");
    let root = t.0.join("proj");
    let store = t.0.join("store");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("main.py"), "print(1)\n").unwrap();
    // With no .gitignore, the built-in excludes are what keep build output out.
    std::fs::create_dir_all(root.join("target/debug")).unwrap();
    std::fs::write(root.join("target/debug/huge.bin"), "x".repeat(4096)).unwrap();
    std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
    std::fs::write(root.join("node_modules/pkg/index.js"), "junk\n").unwrap();

    let s = CheckpointStore::open_at(&root, &store, &CheckpointsConfig::default(), "sess").unwrap();
    assert!(matches!(
        s.snapshot(&SnapshotMeta::baseline()).unwrap(),
        SnapshotOutcome::Created(_)
    ));

    let tracked = s.run(&["ls-files"]).unwrap();
    assert!(tracked.contains("main.py"));
    assert!(!tracked.contains("target/"), "build output was snapshotted");
    assert!(!tracked.contains("node_modules"), "deps were snapshotted");

    // And undo still works here.
    std::fs::write(root.join("main.py"), "print(2)\n").unwrap();
    s.snapshot(&SnapshotMeta::turn("edit", "m", 1, "done"))
        .unwrap();
    let plan = s.plan(CheckpointAction::Undo, None).unwrap();
    s.apply(&plan).unwrap();
    assert_eq!(read(&root.join("main.py")), "print(1)\n");
}

/// Reopening the same project later must still see the earlier session's
/// checkpoints, and ids must keep counting up rather than restarting.
#[test]
fn checkpoints_persist_across_sessions() {
    if !git_available() {
        return;
    }
    let t = temp("persist");
    let root = t.0.join("proj");
    let store = t.0.join("store");
    std::fs::create_dir_all(&root).unwrap();
    let cfg = CheckpointsConfig::default();

    {
        let s = CheckpointStore::open_at(&root, &store, &cfg, "session-one").unwrap();
        std::fs::write(root.join("f.txt"), "a\n").unwrap();
        s.snapshot(&SnapshotMeta::turn("first", "m", 1, "done"))
            .unwrap();
    }

    let s = CheckpointStore::open_at(&root, &store, &cfg, "session-two").unwrap();
    let cps = s.list(10).unwrap();
    assert_eq!(cps.len(), 1);
    assert_eq!(cps[0].label, "first");
    assert_eq!(cps[0].session, "session-one");

    std::fs::write(root.join("f.txt"), "b\n").unwrap();
    let second = match s
        .snapshot(&SnapshotMeta::turn("second", "m", 1, "done"))
        .unwrap()
    {
        SnapshotOutcome::Created(cp) => *cp,
        SnapshotOutcome::Unchanged => panic!("should commit"),
    };
    assert_eq!(second.id, 2, "ids must keep counting across sessions");

    // Yesterday's checkpoint is still restorable today.
    let plan = s.plan(CheckpointAction::Restore(1), None).unwrap();
    s.apply(&plan).unwrap();
    assert_eq!(read(&root.join("f.txt")), "a\n");
}

#[test]
fn show_renders_and_truncates_a_diff() {
    if !git_available() {
        return;
    }
    let t = temp("show");
    let root = t.0.join("proj");
    let store = t.0.join("store");
    std::fs::create_dir_all(&root).unwrap();

    let s = CheckpointStore::open_at(&root, &store, &CheckpointsConfig::default(), "s").unwrap();
    std::fs::write(root.join("a.txt"), "one\n").unwrap();
    s.snapshot(&SnapshotMeta::baseline()).unwrap();
    // Comfortably larger than the 1 KiB floor `open_at` puts under the cap.
    let big: String = (0..400).map(|i| format!("line {i}\n")).collect();
    std::fs::write(root.join("a.txt"), format!("one\ntwo\n{big}")).unwrap();
    s.snapshot(&SnapshotMeta::turn("add a line", "m", 1, "done"))
        .unwrap();

    // No id: falls back to the most recent *turn*.
    let diff = s.show(None).unwrap();
    assert!(diff.contains("add a line"));
    assert!(diff.contains("+two"));
    assert!(s.show(Some(999)).is_err(), "unknown id should error");

    // A tiny cap forces the truncation path, which must not split a codepoint.
    let tiny = CheckpointsConfig {
        diff_max_bytes: 1,
        ..Default::default()
    };
    let s2 = CheckpointStore::open_at(&root, &store, &tiny, "s").unwrap();
    let out = s2.show(None).unwrap();
    assert!(out.contains("truncated"));
}

/// `Path::starts_with` is lexical, so a store path containing `..` could look
/// as though it sat inside the project and be wrongly refused.
#[test]
fn store_path_with_dotdot_is_not_mistaken_for_being_inside() {
    if !git_available() {
        return;
    }
    let t = temp("dotdot");
    let root = t.0.join("proj");
    std::fs::create_dir_all(&root).unwrap();
    // Spelled relative to the project, but actually a sibling of it.
    let store = root.join("../data");
    let s = CheckpointStore::open_at(&root, &store, &CheckpointsConfig::default(), "s")
        .expect("a sibling store must be accepted");
    std::fs::write(root.join("f.txt"), "x\n").unwrap();
    assert!(matches!(
        s.snapshot(&SnapshotMeta::baseline()).unwrap(),
        SnapshotOutcome::Created(_)
    ));
}
