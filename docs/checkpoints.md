# Checkpoints

magai snapshots your working tree after every turn, so the agent's file changes
are always reversible. The snapshots live in a **shadow git repository** under
`$XDG_DATA_HOME/magai/checkpoints/` — **your own repository is never written
to**. No commits, no index changes, no staging, no stash, no moved HEAD, no
pre-commit hooks, no signing.

That means checkpointing:

- works on a dirty tree — nothing is asked of you at startup,
- works alongside your own commits, rebases and branch switches,
- works in directories that aren't git repositories at all,
- covers files written by `shell_command` (a `sed -i`, a build script, a
  runaway `cargo fix`), because it snapshots the *tree* rather than tool calls.

## Commands

| Command | What it does |
|---|---|
| `/checkpoints` | List saved checkpoints, newest first |
| `/diff [n]` | Show what a turn changed (defaults to the last turn) |
| `/undo` | Revert the last turn's file changes |
| `/redo` | Re-apply what `/undo` reverted |
| `/restore <n>` | Reset the whole working tree to checkpoint `n` |

`/undo` and `/restore` show a confirmation card with the file list first.
Nothing is written until you press `y`.

## `/undo` vs `/restore`

`/undo` is **surgical**: it reverse-applies only the patch that turn introduced.
If you edited other files yourself while the agent worked, those edits are left
alone. If your own edit collides with the agent's in the same place, the undo is
**refused** and nothing is written — magai tells you which files collided. Use
`/restore` if you want the blunt version.

`/restore` is **whole-tree**: it puts every tracked file back exactly as it was
at that checkpoint, which does discard your own concurrent edits. It is still
safe: a snapshot is always taken immediately before, and the message tells you
which checkpoint number gets you back.

Every destructive action snapshots first, so nothing is ever unrecoverable.

## What is and isn't covered

Covered: any file in the project that isn't ignored.

Not covered:

- **Ignored files.** Snapshots respect your `.gitignore` (and `.git/info/exclude`,
  which is copied in), plus a built-in list of build output and caches. Agent
  edits to an ignored file cannot be undone. Add `[checkpoints] exclude` to widen
  the list; set `use_default_excludes = false` to drop the built-in one.
- **Submodules and nested repositories.** Recorded as a pointer; their contents
  are not snapshotted or restored.
- **Permissions beyond the executable bit.** Modes, ownership, ACLs and extended
  attributes are not tracked, so a `chmod 600` is not rolled back.
- **Anything outside the project root.**

`/undo` reverts the last *turn*, not the last *thing that happened*. If you ran
`git checkout` between turns, the next snapshot records that as a change, and an
`/undo` afterwards will refuse rather than fight you for it.

## Where snapshots live

`$XDG_DATA_HOME/magai/checkpoints/<project>-<hash>/git` — one shadow repository
per project root, keyed by the canonical path so two projects with the same name
never share a store. Old checkpoints beyond `[checkpoints] keep` (200 by default)
are pruned at session start. Configure it all under `[checkpoints]`; see
`docs/config.example.toml`.

To disable entirely:

```toml
[checkpoints]
enabled = false
```

## Migrating from the old git checkpoints

Earlier versions of magai committed directly into your repository (a
`magai-checkpoint` commit per turn), and `/undo` was `git reset --hard HEAD~1`.
Those commits are harmless to the new system — it only reads the working tree —
but you may want to clean them up.

```sh
# Find them:
git log --oneline --grep='^magai-checkpoint$'

# If they are your most recent commits and you want the work back as
# uncommitted changes (this is what the old /squash did):
git reset --soft <sha-of-the-commit-before-the-first-magai-checkpoint>

# If they are interleaved with your own commits, drop just those lines:
git rebase --interactive <base>
```

**Check for a stranded stash.** The old dirty-workspace prompt offered to
"stash & enable undo" — it created a stash and never popped it, so real work may
still be sitting there:

```sh
git stash list | grep magai-user-stash
git stash show -p 'stash@{N}'   # inspect first
git stash pop 'stash@{N}'
```

magai will not do any of this for you: the whole point of the new design is that
it does not touch your repository, and that includes not "helpfully" rewriting
your history.
