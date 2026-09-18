//! Per-turn snapshots of the working tree, kept in a **shadow git repository**
//! that lives outside the project (under `$XDG_DATA_HOME/magai/checkpoints`).
//!
//! Every git invocation passes `--git-dir=<shadow> --work-tree=<project>`, so
//! the project's own repository is never read or written: no commits, no index,
//! no HEAD, no stash, no reflog, no hooks, no signing. Checkpointing therefore
//! works on a dirty tree, alongside the user's own commits, and in directories
//! that are not git repositories at all.
//!
//! Because snapshots capture the *tree* rather than tool calls, files written by
//! `shell_command` are covered just like `write_file` and `edit_file` edits.

mod format;
mod parse;

pub use format::{render_list, render_stat};

use crate::config::CheckpointsConfig;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Built-in exclude list. It matters most in a project with no `.gitignore`,
/// where `git add -A` would otherwise try to snapshot `target/` wholesale.
/// Deliberately absent: `.vscode/`, `.env`, `tmp/` — too often meaningful, and
/// the agent editing them is exactly what you want to be able to undo.
const DEFAULT_EXCLUDES: &str = "\
/.git/
target/
build/
dist/
out/
node_modules/
bower_components/
.venv/
venv/
__pycache__/
*.py[cod]
.mypy_cache/
.pytest_cache/
.ruff_cache/
.tox/
*.egg-info/
.next/
.nuxt/
.svelte-kit/
.turbo/
.parcel-cache/
.gradle/
.stack-work/
.cache/
.direnv/
.terraform/
coverage/
.nyc_output/
*.log
*.o
*.obj
*.so
*.dylib
*.dll
*.a
*.class
*.jar
.DS_Store
Thumbs.db
*.swp
*.swo
*~
.idea/
.ipynb_checkpoints/
";

/// Config block written into the shadow repository. Regenerated on every
/// `open`, appended rather than replacing the file so the values `git init`
/// probed against the filesystem (`filemode`, `ignorecase`, `precomposeunicode`)
/// survive — those must reflect reality, not our preferences.
const CONFIG_BEGIN: &str = "# >>> magai managed — regenerated on startup";
const CONFIG_END: &str = "# <<< magai managed";

/// Highest-precedence attributes, above the worktree's `.gitattributes`.
/// `-filter` is the important one: without it a git-lfs project's snapshots
/// store LFS pointers and a restore smudges garbage into the working tree.
/// `-diff` is deliberately NOT set — undo needs textual patches.
const ATTRIBUTES: &str = "* -text -filter -ident -export-subst\n";

const BRANCH: &str = "refs/heads/magai";
const REDO_REF: &str = "refs/magai/redo";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointAction {
    Undo,
    Redo,
    Restore(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointKind {
    Baseline,
    Turn,
    Undo,
    Redo,
    Restore,
}

impl CheckpointKind {
    fn as_str(self) -> &'static str {
        match self {
            CheckpointKind::Baseline => "baseline",
            CheckpointKind::Turn => "turn",
            CheckpointKind::Undo => "undo",
            CheckpointKind::Redo => "redo",
            CheckpointKind::Restore => "restore",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeStatus {
    Added,
    Modified,
    Deleted,
    TypeChanged,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Checkpoint {
    pub id: u64,
    pub sha: String,
    pub parent: Option<String>,
    pub unix_time: i64,
    pub kind: CheckpointKind,
    pub outcome: Option<String>,
    pub label: String,
    pub model: Option<String>,
    pub session: String,
    pub turn: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileStat {
    pub status: ChangeStatus,
    pub path: String,
    pub added: usize,
    pub removed: usize,
    pub binary: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiffStat {
    pub files: Vec<FileStat>,
    pub insertions: usize,
    pub deletions: usize,
}

/// Inputs for one snapshot, grouped so no signature trips
/// `clippy::too_many_arguments`.
#[derive(Debug, Clone)]
pub struct SnapshotMeta {
    pub kind: CheckpointKind,
    pub outcome: Option<String>,
    pub label: String,
    pub model: Option<String>,
    pub turn: Option<usize>,
}

impl SnapshotMeta {
    pub fn baseline() -> Self {
        Self {
            kind: CheckpointKind::Baseline,
            outcome: None,
            label: "session start".to_string(),
            model: None,
            turn: None,
        }
    }

    pub fn turn(label: &str, model: &str, turn: usize, outcome: &str) -> Self {
        Self {
            kind: CheckpointKind::Turn,
            outcome: Some(outcome.to_string()),
            label: label.to_string(),
            model: Some(model.to_string()),
            turn: Some(turn),
        }
    }

    /// A snapshot taken to make an undo/redo/restore itself reversible.
    pub fn internal(kind: CheckpointKind, label: &str) -> Self {
        Self {
            kind,
            outcome: None,
            label: label.to_string(),
            model: None,
            turn: None,
        }
    }
}

#[derive(Debug)]
pub enum SnapshotOutcome {
    Created(Box<Checkpoint>),
    /// The tree was identical to the previous checkpoint, so nothing was
    /// committed — a turn that changed no files leaves no row behind.
    Unchanged,
}

/// What a confirmation card shows before a destructive action runs.
#[derive(Debug, Clone)]
pub struct Plan {
    pub action: CheckpointAction,
    pub target: Checkpoint,
    pub stat: DiffStat,
    /// `Some(reason)` means the action is refused and must not be applied.
    pub blocked: Option<String>,
}

#[derive(Debug)]
pub enum CheckpointError {
    Spawn(std::io::Error),
    Git {
        cmd: String,
        stderr: String,
    },
    /// The patch did not apply cleanly; nothing was written.
    Conflict {
        paths: Vec<String>,
    },
    NotFound(u64),
    /// Nothing to undo or redo.
    Empty,
    Unsupported(String),
}

impl std::fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckpointError::Spawn(e) => write!(f, "could not run git: {e}"),
            CheckpointError::Git { cmd, stderr } => {
                write!(f, "git {cmd} failed: {}", stderr.trim())
            }
            CheckpointError::Conflict { paths } => write!(
                f,
                "your own edits collide with that turn in {} — nothing was changed. \
                 Use /restore to reset the whole tree instead.",
                paths.join(", ")
            ),
            CheckpointError::NotFound(id) => write!(f, "no checkpoint #{id}"),
            CheckpointError::Empty => write!(f, "nothing to do"),
            CheckpointError::Unsupported(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for CheckpointError {}

type Result<T> = std::result::Result<T, CheckpointError>;

pub struct CheckpointStore {
    git_dir: PathBuf,
    work_tree: PathBuf,
    session: String,
    keep: usize,
    max_list: usize,
    diff_max_bytes: usize,
}

impl CheckpointStore {
    /// Resolves the project root and store location from the environment, then
    /// defers to [`CheckpointStore::open_at`].
    pub fn open(cfg: &CheckpointsConfig, session: &str) -> Result<Self> {
        let root = match &cfg.root {
            Some(r) => PathBuf::from(crate::config::expand_tilde(r)),
            None => {
                let cwd = std::env::current_dir()
                    .map_err(|e| CheckpointError::Unsupported(format!("no working dir: {e}")))?;
                project_root(&cwd)
            }
        };
        let store = match &cfg.store {
            Some(s) => PathBuf::from(crate::config::expand_tilde(s)),
            None => default_store_dir(),
        };
        Self::open_at(&root, &store, cfg, session)
    }

    /// Test seam: explicit project root and store directory, reading no
    /// environment.
    pub fn open_at(
        root: &Path,
        store_dir: &Path,
        cfg: &CheckpointsConfig,
        session: &str,
    ) -> Result<Self> {
        // A symlinked root would otherwise get a second, unrelated store.
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

        // Resolve the store before comparing: `Path::starts_with` is purely
        // lexical, so an unresolved `.../proj/../data` would look as though it
        // sat inside `.../proj` and trip the guard below.
        std::fs::create_dir_all(store_dir).map_err(CheckpointError::Spawn)?;
        let store_dir = store_dir
            .canonicalize()
            .unwrap_or_else(|_| store_dir.to_path_buf());
        let git_dir = store_dir.join(slug_for(&root)).join("git");

        // A store inside the worktree would snapshot itself, forever.
        if git_dir.starts_with(&root) {
            return Err(CheckpointError::Unsupported(
                "checkpoint store is inside the project; set [checkpoints] store".to_string(),
            ));
        }

        let store = Self {
            git_dir,
            work_tree: root,
            session: session.to_string(),
            keep: cfg.keep.max(1),
            max_list: cfg.max_list.max(1),
            diff_max_bytes: cfg.diff_max_bytes.max(1024),
        };
        store.init(cfg)?;
        Ok(store)
    }

    fn init(&self, cfg: &CheckpointsConfig) -> Result<()> {
        if !self.git_dir.join("HEAD").exists() {
            if let Some(parent) = self.git_dir.parent() {
                std::fs::create_dir_all(parent).map_err(CheckpointError::Spawn)?;
            }
            // `--bare` keeps `git init` from creating anything in the project.
            run_raw(Command::new("git").args([
                "init",
                "--bare",
                "--quiet",
                "--",
                &self.git_dir.to_string_lossy(),
            ]))?;
            // Avoids depending on the user's `init.defaultBranch`.
            self.run(&["symbolic-ref", "HEAD", BRANCH])?;
        }

        std::fs::create_dir_all(self.git_dir.join("info")).map_err(CheckpointError::Spawn)?;
        // An empty directory named as the hook path means no hook ever runs.
        std::fs::create_dir_all(self.git_dir.join("no-hooks")).map_err(CheckpointError::Spawn)?;

        self.write_config()?;
        std::fs::write(self.git_dir.join("info").join("attributes"), ATTRIBUTES)
            .map_err(CheckpointError::Spawn)?;

        // The shadow cannot see the project's own .git/info/exclude, so copy it.
        let project_exclude = cfg
            .copy_project_exclude
            .then(|| std::fs::read_to_string(self.work_tree.join(".git/info/exclude")).ok())
            .flatten();
        std::fs::write(
            self.git_dir.join("info").join("exclude"),
            exclude_file(
                cfg.use_default_excludes,
                &cfg.exclude,
                project_exclude.as_deref(),
            ),
        )
        .map_err(CheckpointError::Spawn)?;
        Ok(())
    }

    /// Rewrites our managed block in the shadow's config, preserving whatever
    /// `git init` probed about the filesystem.
    fn write_config(&self) -> Result<()> {
        let path = self.git_dir.join("config");
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let mut kept = String::new();
        let mut skipping = false;
        for line in existing.lines() {
            if line.trim() == CONFIG_BEGIN {
                skipping = true;
            }
            if !skipping {
                kept.push_str(line);
                kept.push('\n');
            }
            if line.trim() == CONFIG_END {
                skipping = false;
            }
        }

        let hooks = self.git_dir.join("no-hooks");
        let managed = format!(
            "{CONFIG_BEGIN}\n\
             [core]\n\
             \tbare = false\n\
             \tautocrlf = false\n\
             \teol = lf\n\
             \tsafecrlf = false\n\
             \tlogAllRefUpdates = true\n\
             \tuntrackedCache = true\n\
             \tfsync = none\n\
             \thooksPath = {}\n\
             [user]\n\
             \tname = magai\n\
             \temail = magai@localhost\n\
             [commit]\n\
             \tgpgSign = false\n\
             [tag]\n\
             \tgpgSign = false\n\
             [gc]\n\
             \tauto = 0\n\
             [diff]\n\
             \trenames = false\n\
             [advice]\n\
             \taddEmbeddedRepo = false\n\
             [submodule]\n\
             \trecurse = false\n\
             {CONFIG_END}\n",
            hooks.to_string_lossy()
        );
        std::fs::write(&path, format!("{kept}{managed}")).map_err(CheckpointError::Spawn)
    }

    /// The command envelope. Scrubs every inherited git environment variable:
    /// magai launched from a git hook or `git rebase --exec` would otherwise
    /// have its snapshots redirected into the user's repository — precisely the
    /// failure this module exists to prevent.
    fn command(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.work_tree)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_NAMESPACE")
            .env_remove("GIT_PREFIX")
            .env_remove("GIT_CONFIG")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            // Keeps `git apply` stderr parseable by `parse::conflict_paths`.
            .env("LC_ALL", "C")
            .args(["--no-pager", "-c", "core.quotepath=false"])
            .arg(format!("--git-dir={}", self.git_dir.display()))
            .arg(format!("--work-tree={}", self.work_tree.display()));
        cmd
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let out = run_raw(self.command().args(args))?;
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// Like `run`, but a non-zero exit is reported as `Ok(None)` rather than an
    /// error — for queries where "no such ref" is an ordinary answer.
    fn try_run(&self, args: &[&str]) -> Option<String> {
        let out = self.command().args(args).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn run_bytes(&self, args: &[&str]) -> Result<Vec<u8>> {
        Ok(run_raw(self.command().args(args))?.stdout)
    }

    // ── snapshots ────────────────────────────────────────────────────────────

    pub fn snapshot(&self, meta: &SnapshotMeta) -> Result<SnapshotOutcome> {
        self.run(&[
            "add",
            "-A",
            "--ignore-errors",
            "--no-warn-embedded-repo",
            "--",
        ])?;
        let tree = self.run(&["write-tree"])?.trim().to_string();

        let parent = self.try_run(&["rev-parse", "--quiet", "--verify", BRANCH]);
        let parent_tree = self.try_run(&[
            "rev-parse",
            "--quiet",
            "--verify",
            &format!("{BRANCH}^{{tree}}"),
        ]);

        // Empty-commit guard by tree comparison: cheaper than a diff, and it
        // avoids the "diff-index HEAD behaves differently on the root commit"
        // wart that the old implementation tripped over.
        if parent_tree.as_deref() == Some(tree.as_str()) {
            return Ok(SnapshotOutcome::Unchanged);
        }

        let next_id = self.head()?.map(|cp| cp.id + 1).unwrap_or(1);
        let message = build_message(meta, next_id, &self.session);

        // `commit-tree` runs no hooks, honours no `commit.gpgsign`, opens no
        // editor and needs no HEAD.
        let mut args = vec!["commit-tree".to_string(), tree];
        if let Some(p) = &parent {
            args.push("-p".to_string());
            args.push(p.clone());
        }
        args.push("-F".to_string());
        args.push("-".to_string());
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let sha = self.run_with_stdin(&arg_refs, message.as_bytes())?;
        let sha = sha.trim().to_string();

        // Passing the old value makes this a compare-and-swap, so a second
        // magai in the same project fails loudly instead of losing a snapshot.
        self.run(&[
            "update-ref",
            "-m",
            meta.kind.as_str(),
            BRANCH,
            &sha,
            parent.as_deref().unwrap_or(""),
        ])?;

        // A fresh turn invalidates any pending redo.
        if meta.kind == CheckpointKind::Turn {
            let _ = self.try_run(&["update-ref", "-d", REDO_REF]);
        }

        let cp = self
            .resolve_sha(&sha)?
            .ok_or_else(|| CheckpointError::Unsupported("snapshot vanished".to_string()))?;
        Ok(SnapshotOutcome::Created(Box::new(cp)))
    }

    fn run_with_stdin(&self, args: &[&str], stdin: &[u8]) -> Result<String> {
        let mut child = self
            .command()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(CheckpointError::Spawn)?;
        child
            .stdin
            .as_mut()
            .ok_or_else(|| CheckpointError::Unsupported("no stdin".to_string()))?
            .write_all(stdin)
            .map_err(CheckpointError::Spawn)?;
        let out = child.wait_with_output().map_err(CheckpointError::Spawn)?;
        if !out.status.success() {
            return Err(CheckpointError::Git {
                cmd: args.first().unwrap_or(&"?").to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    // ── queries ──────────────────────────────────────────────────────────────

    pub fn list(&self, limit: usize) -> Result<Vec<Checkpoint>> {
        let limit = limit.min(self.max_list).to_string();
        let Some(raw) = self.try_run(&[
            "log",
            "-n",
            &limit,
            &format!("--format=%H{FS}%P{FS}%ct{FS}%B%x00", FS = parse::FS),
            BRANCH,
        ]) else {
            return Ok(Vec::new()); // no checkpoints yet
        };
        Ok(parse::parse_log(&raw))
    }

    pub fn head(&self) -> Result<Option<Checkpoint>> {
        Ok(self.list(1)?.into_iter().next())
    }

    pub fn resolve(&self, id: u64) -> Result<Checkpoint> {
        self.list(self.max_list)?
            .into_iter()
            .find(|cp| cp.id == id)
            .ok_or(CheckpointError::NotFound(id))
    }

    fn resolve_sha(&self, sha: &str) -> Result<Option<Checkpoint>> {
        let Some(raw) = self.try_run(&[
            "log",
            "-n",
            "1",
            &format!("--format=%H{FS}%P{FS}%ct{FS}%B%x00", FS = parse::FS),
            sha,
        ]) else {
            return Ok(None);
        };
        Ok(parse::parse_log(&raw).into_iter().next())
    }

    /// Stat between two commits. `from` may be a commit or a tree-ish.
    pub fn stat(&self, from: &str, to: &str) -> Result<DiffStat> {
        let names = self.run(&[
            "diff-tree",
            "-r",
            "--no-renames",
            "--name-status",
            "-z",
            from,
            to,
        ])?;
        let nums = self.run(&[
            "diff-tree",
            "-r",
            "--no-renames",
            "--numstat",
            "-z",
            from,
            to,
        ])?;
        Ok(parse::merge_stat(
            parse::parse_name_status_z(&names),
            parse::parse_numstat_z(&nums),
        ))
    }

    /// The stat introduced by a checkpoint, relative to its parent.
    fn stat_of(&self, cp: &Checkpoint) -> Result<DiffStat> {
        match &cp.parent {
            Some(parent) => self.stat(parent, &cp.sha),
            // A root commit is diffed against the empty tree.
            None => {
                let empty = self.run(&["hash-object", "-t", "tree", "/dev/null"])?;
                self.stat(empty.trim(), &cp.sha)
            }
        }
    }

    /// The patch a checkpoint introduced.
    ///
    /// `--binary` is required or binary files render as "Binary files differ"
    /// and cannot be applied. The explicit prefixes defeat a user's global
    /// `diff.noprefix`/`diff.mnemonicPrefix`, which would silently break `-p1`.
    fn patch_of(&self, cp: &Checkpoint) -> Result<Vec<u8>> {
        let parent = cp.parent.clone().unwrap_or_else(|| {
            self.run(&["hash-object", "-t", "tree", "/dev/null"])
                .unwrap_or_default()
                .trim()
                .to_string()
        });
        self.run_bytes(&[
            "-c",
            "diff.external=",
            "diff",
            "--binary",
            "--full-index",
            "--no-renames",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--ignore-submodules=all",
            "-U3",
            &parent,
            &cp.sha,
            "--",
        ])
    }

    /// Rendered diff text for `/diff [n]`, truncated on a char boundary.
    pub fn show(&self, id: Option<u64>) -> Result<String> {
        let cp = match id {
            Some(id) => self.resolve(id)?,
            None => self
                .list(self.max_list)?
                .into_iter()
                .find(|c| c.kind == CheckpointKind::Turn)
                .ok_or(CheckpointError::Empty)?,
        };
        let raw = self.patch_of(&cp)?;
        let text = String::from_utf8_lossy(&raw).to_string();
        if text.trim().is_empty() {
            return Ok(format!("#{} {} — no file changes", cp.id, cp.label));
        }
        let header = format!("#{} {} ({})\n", cp.id, cp.label, cp.kind.as_str());
        if text.len() <= self.diff_max_bytes {
            return Ok(format!("{header}{text}"));
        }
        let cut = crate::ui::text::floor_char_boundary(&text, self.diff_max_bytes);
        Ok(format!(
            "{header}{}\n… diff truncated at {} bytes",
            &text[..cut],
            self.diff_max_bytes
        ))
    }

    // ── undo / redo / restore ────────────────────────────────────────────────

    /// Works out what an action would do, so the UI can confirm it first.
    /// `cursor` is the sha already undone this session, letting a second
    /// consecutive `/undo` step one turn further back.
    pub fn plan(&self, action: CheckpointAction, cursor: Option<&str>) -> Result<Plan> {
        let target = match action {
            CheckpointAction::Undo => {
                let all = self.list(self.max_list)?;
                let start = match cursor {
                    Some(sha) => all
                        .iter()
                        .position(|c| c.sha == sha)
                        .map(|i| i + 1)
                        .unwrap_or(0),
                    None => 0,
                };
                all.into_iter()
                    .skip(start)
                    .find(|c| c.kind == CheckpointKind::Turn)
                    .ok_or(CheckpointError::Empty)?
            }
            CheckpointAction::Redo => {
                let sha = self
                    .try_run(&["rev-parse", "--quiet", "--verify", REDO_REF])
                    .ok_or(CheckpointError::Empty)?;
                self.resolve_sha(&sha)?.ok_or(CheckpointError::Empty)?
            }
            CheckpointAction::Restore(id) => self.resolve(id)?,
        };

        let stat = match action {
            // A restore is measured against the tree as it stands now.
            CheckpointAction::Restore(_) => {
                let tree = self.current_tree()?;
                self.stat(&tree, &target.sha)?
            }
            _ => self.stat_of(&target)?,
        };

        // A dry run decides whether the surgical actions can proceed at all.
        let blocked = match action {
            CheckpointAction::Undo | CheckpointAction::Redo => {
                let reverse = matches!(action, CheckpointAction::Undo);
                match self.check_patch(&target, reverse) {
                    Ok(()) => None,
                    Err(CheckpointError::Conflict { paths }) => Some(format!(
                        "your own edits collide in {} — nothing will be changed",
                        paths.join(", ")
                    )),
                    Err(e) => Some(e.to_string()),
                }
            }
            CheckpointAction::Restore(_) => None,
        };

        Ok(Plan {
            action,
            target,
            stat,
            blocked,
        })
    }

    /// Stages the working tree into a throwaway tree object, so a restore can
    /// be measured against what is actually on disk right now.
    fn current_tree(&self) -> Result<String> {
        self.run(&[
            "add",
            "-A",
            "--ignore-errors",
            "--no-warn-embedded-repo",
            "--",
        ])?;
        Ok(self.run(&["write-tree"])?.trim().to_string())
    }

    /// `git apply --check` — verifies every hunk against what is on disk and
    /// writes nothing. This is what makes a refused undo atomic.
    fn check_patch(&self, cp: &Checkpoint, reverse: bool) -> Result<()> {
        let patch = self.patch_of(cp)?;
        if patch.is_empty() {
            return Err(CheckpointError::Empty);
        }
        let mut args = vec!["apply"];
        if reverse {
            args.push("--reverse");
        }
        args.extend_from_slice(&["--check", "--whitespace=nowarn", "-"]);
        match self.run_with_stdin(&args, &patch) {
            Ok(_) => Ok(()),
            Err(CheckpointError::Git { stderr, .. }) => {
                let paths = parse::conflict_paths(&stderr);
                Err(if paths.is_empty() {
                    CheckpointError::Git {
                        cmd: "apply --check".to_string(),
                        stderr,
                    }
                } else {
                    CheckpointError::Conflict { paths }
                })
            }
            Err(e) => Err(e),
        }
    }

    /// Applies a previously computed plan. Always snapshots first, so the
    /// action itself is reversible; the returned id is that safety snapshot,
    /// which `/restore` can use to get back to the pre-action tree.
    pub fn apply(&self, plan: &Plan) -> Result<(DiffStat, Option<u64>)> {
        if let Some(reason) = &plan.blocked {
            return Err(CheckpointError::Unsupported(reason.clone()));
        }

        let (kind, label) = match plan.action {
            CheckpointAction::Undo => (CheckpointKind::Undo, "before undo"),
            CheckpointAction::Redo => (CheckpointKind::Redo, "before redo"),
            CheckpointAction::Restore(_) => (CheckpointKind::Restore, "before restore"),
        };
        let safety = match self.snapshot(&SnapshotMeta::internal(kind, label))? {
            SnapshotOutcome::Created(cp) => Some(cp.id),
            SnapshotOutcome::Unchanged => None,
        };

        match plan.action {
            CheckpointAction::Undo | CheckpointAction::Redo => {
                let reverse = matches!(plan.action, CheckpointAction::Undo);
                // Re-check: the pre-snapshot above must not have changed
                // anything, but the tree is only ours between these two calls.
                self.check_patch(&plan.target, reverse)?;
                let patch = self.patch_of(&plan.target)?;
                let mut args = vec!["apply"];
                if reverse {
                    args.push("--reverse");
                }
                args.extend_from_slice(&["--whitespace=nowarn", "-"]);
                self.run_with_stdin(&args, &patch)?;

                if reverse {
                    self.run(&["update-ref", REDO_REF, &plan.target.sha])?;
                } else {
                    let _ = self.try_run(&["update-ref", "-d", REDO_REF]);
                }
            }
            CheckpointAction::Restore(_) => {
                // Not `reset --hard`: that moves the branch and breaks the
                // append-only log that lets /redo survive a /restore. The
                // pre-restore snapshot above is load-bearing — it is what makes
                // everything `clean` removes recoverable.
                self.run(&["read-tree", "-u", "--reset", &plan.target.sha])?;
                // No -x (ignored files survive) and no -ff (nested repos survive).
                self.run(&["clean", "-fdq", "--"])?;
            }
        }

        Ok((plan.stat.clone(), safety))
    }

    // ── maintenance ──────────────────────────────────────────────────────────

    /// Drops the oldest checkpoints beyond `keep`. Linear history cannot have
    /// its tail removed in place — children pin their parents by sha — so the
    /// survivors are re-committed onto a new root, reusing their existing tree
    /// objects (no working-tree I/O) and preserving their ids and timestamps.
    pub fn prune(&self) -> Result<usize> {
        let all = self.list(usize::MAX)?;
        if all.len() <= self.keep {
            return Ok(0);
        }
        let dropped = all.len() - self.keep;
        let survivors: Vec<Checkpoint> = all.into_iter().take(self.keep).rev().collect();
        let old_tip = self
            .try_run(&["rev-parse", "--quiet", "--verify", BRANCH])
            .unwrap_or_default();

        let mut parent: Option<String> = None;
        for cp in &survivors {
            let tree = self.run(&["rev-parse", &format!("{}^{{tree}}", cp.sha)])?;
            let meta = SnapshotMeta {
                kind: cp.kind,
                outcome: cp.outcome.clone(),
                label: cp.label.clone(),
                model: cp.model.clone(),
                turn: cp.turn,
            };
            let message = build_message(&meta, cp.id, &cp.session);
            let mut args = vec!["commit-tree".to_string(), tree.trim().to_string()];
            if let Some(p) = &parent {
                args.push("-p".to_string());
                args.push(p.clone());
            }
            args.push("-F".to_string());
            args.push("-".to_string());
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let date = format!("@{} +0000", cp.unix_time);
            let mut cmd = self.command();
            cmd.args(&refs)
                .env("GIT_AUTHOR_DATE", &date)
                .env("GIT_COMMITTER_DATE", &date)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = cmd.spawn().map_err(CheckpointError::Spawn)?;
            child
                .stdin
                .as_mut()
                .ok_or_else(|| CheckpointError::Unsupported("no stdin".to_string()))?
                .write_all(message.as_bytes())
                .map_err(CheckpointError::Spawn)?;
            let out = child.wait_with_output().map_err(CheckpointError::Spawn)?;
            if !out.status.success() {
                return Err(CheckpointError::Git {
                    cmd: "commit-tree".to_string(),
                    stderr: String::from_utf8_lossy(&out.stderr).to_string(),
                });
            }
            parent = Some(String::from_utf8_lossy(&out.stdout).trim().to_string());
        }

        if let Some(tip) = parent {
            self.run(&["update-ref", BRANCH, &tip, &old_tip])?;
            let _ = self.try_run(&["update-ref", "-d", REDO_REF]);
            let _ = self.try_run(&["reflog", "expire", "--expire=now", "--all"]);
            let _ = self.try_run(&["gc", "--prune=now", "--quiet"]);
        }
        Ok(dropped)
    }
}

// ── pure helpers ─────────────────────────────────────────────────────────────

fn run_raw(cmd: &mut Command) -> Result<Output> {
    let out = cmd.output().map_err(CheckpointError::Spawn)?;
    if !out.status.success() {
        return Err(CheckpointError::Git {
            cmd: format!("{:?}", cmd.get_program()),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        });
    }
    Ok(out)
}

/// `$XDG_DATA_HOME/magai/checkpoints`, mirroring `memory::default_db_path`.
fn default_store_dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    base.join("magai").join("checkpoints")
}

/// Walks up looking for a `.git` entry — a **file** counts too, so linked
/// worktrees and submodule checkouts resolve correctly — and falls back to
/// `cwd`, which is also the not-a-repository case.
///
/// Deliberately not `git rev-parse --show-toplevel`: that would read the user's
/// repository, which this module never does.
pub(crate) fn project_root(cwd: &Path) -> PathBuf {
    let mut dir = cwd;
    loop {
        if dir.join(".git").exists() {
            return dir.to_path_buf();
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return cwd.to_path_buf(),
        }
    }
}

/// FNV-1a, hand-rolled on purpose: `DefaultHasher`'s output is **not stable
/// across Rust releases**, and a changed hash would orphan every checkpoint a
/// user already has.
pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// A readable directory name plus a path hash, so same-named projects in
/// different places never share a store.
pub(crate) fn slug_for(root: &Path) -> String {
    let name: String = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "root".to_string())
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(32)
        .collect();
    let name = name.trim_matches('-');
    let name = if name.is_empty() { "root" } else { name };
    format!(
        "{name}-{:016x}",
        fnv1a64(root.as_os_str().as_encoded_bytes())
    )
}

/// Collapses a label to one safe line: our log format uses US and NUL as
/// separators, so neither may survive into a commit message.
pub(crate) fn sanitize_label(s: &str) -> String {
    let flat: String = s
        .chars()
        .map(|c| {
            if c == '\n' || c == '\r' || c == '\u{1f}' || c == '\0' {
                ' '
            } else {
                c
            }
        })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= 120 {
        return flat;
    }
    flat.chars().take(119).chain(std::iter::once('…')).collect()
}

pub(crate) fn build_message(meta: &SnapshotMeta, id: u64, session: &str) -> String {
    let label = sanitize_label(&meta.label);
    let mut msg = format!(
        "magai: {} — {}\n\nMagai-Version: 1\nMagai-Checkpoint: {id}\nMagai-Kind: {}\n",
        meta.kind.as_str(),
        if label.is_empty() {
            "(no label)"
        } else {
            &label
        },
        meta.kind.as_str(),
    );
    if !label.is_empty() {
        msg.push_str(&format!("Magai-Label: {label}\n"));
    }
    if let Some(outcome) = &meta.outcome {
        msg.push_str(&format!("Magai-Outcome: {}\n", sanitize_label(outcome)));
    }
    if let Some(model) = &meta.model {
        msg.push_str(&format!("Magai-Model: {}\n", sanitize_label(model)));
    }
    if let Some(turn) = meta.turn {
        msg.push_str(&format!("Magai-Turn: {turn}\n"));
    }
    msg.push_str(&format!("Magai-Session: {}\n", sanitize_label(session)));
    msg
}

pub(crate) fn exclude_file(
    use_defaults: bool,
    extra: &[String],
    project_exclude: Option<&str>,
) -> String {
    let mut out = String::from("# magai checkpoint excludes — regenerated on startup\n");
    if use_defaults {
        out.push_str(DEFAULT_EXCLUDES);
    } else {
        // The store must never try to snapshot the project's own repository.
        out.push_str("/.git/\n");
    }
    for pattern in extra {
        out.push_str(pattern.trim());
        out.push('\n');
    }
    if let Some(project) = project_exclude {
        out.push_str("# from the project's .git/info/exclude\n");
        out.push_str(project);
        if !project.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests;
