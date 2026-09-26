# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`magai` is a terminal coding-agent application — a TUI (built on `ratatui`/`crossterm`)
that drives an AI agent (via `rig-core`) capable of calling tools (file
read/write/edit, shell, git, grep, web fetch, MCP servers) against the local
project, with an approval gate for dangerous operations. Conceptually it's a
Claude-Code-like agent harness, written in Rust.

## CLI vs TUI

`main.rs` parses `cli::Cli` first: a subcommand runs in the terminal and exits,
bare `magai` opens the TUI. `cli.rs` currently hosts `magai mcp add/list/get/
remove`, which edit `~/.config/magai/config.toml` through `toml_edit` so
comments and formatting survive; `add_server`/`remove_server` operate on a
`DocumentMut` and are unit-tested without touching the filesystem. Subcommands
return `Result<(), String>` and `main` prints the message to stderr and exits 1.

`magai init` (and `/config` in the TUI, which suspends the alternate screen to
run it) is the interactive config wizard in `setup.rs`, built on `inquire`.
Each question shows the file's current value as the placeholder (Enter keeps
it) and the serde default in the help line (`-` resets by removing the key);
defaults come from `toml::from_str::<Config>("")`, not `Config::default()`,
which is derived and zeroes numeric fields. Keys left at their default are not
written, and the result is validated against `Config` before saving. Questions
go through the `Prompter` trait so tests script answers. A bare `magai` with no
config file offers to run it first (`setup::offer_first_run`). When adding a
config setting users are likely to change, add a question for it there.

## Common commands

```sh
cargo build              # debug build
cargo build --release    # release build
cargo run                # build and launch the TUI
cargo test               # run tests (small #[cfg(test)] modules — see below)
cargo test <name>        # run a single test by name/substring
cargo clippy             # lint
cargo fmt                # format
```

A handful of `#[cfg(test)] mod tests` modules cover the pure, dependency-free
logic: `tools::glob`, `slash_commands`, `config`, and `ui::text`. Add new ones
the same way for any newly-extracted pure logic — there's no separate test
directory or harness.

CI (`.github/workflows/ci.yml`) runs on every PR and push to `main`: `cargo fmt
--all --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo
test --locked`, and a release build. Clippy is a hard gate, so a new warning
fails the PR — fix it rather than allowing the lint. `--locked` is used
throughout, so a dependency change must commit the updated `Cargo.lock`.

Tagging `v<version>` triggers `.github/workflows/release.yml`, which refuses to
publish unless the tag matches the `Cargo.toml` version, then attaches
`.tar.gz` + `.sha256` binaries for linux-x86_64 and macOS (arm64, x86_64) to a
release that stays a draft until every platform has uploaded.

There is no README or lint config in the repo currently — don't assume
conventions beyond what's in the source.

## Architecture

### Two async halves connected by channels

`main.rs` spawns two long-lived tasks that never call each other directly —
they communicate purely through `tokio::mpsc` channels:

- **`ai::run_agent`** (`src/ai.rs`, plus `ai/providers.rs`, `ai/stream.rs`,
  and `ai/background.rs`) — owns the LLM agent, conversation history, tool server, and
  hook runner. Receives `AgentCommand`s (user message, model switch,
  approve/deny tool call, clear, undo, …) and emits `AiEvent`s (token,
  tool-call start/result, errors, …).
- **`ui::App`** (`src/ui/mod.rs`, plus `ui/render.rs`, `ui/input.rs`, and
  `ui/text.rs`) — the ratatui event loop (`App::run`). Owns all rendering and
  input handling, sends `AgentCommand`s, and drains `AiEvent`s each frame via
  `poll_ai_events`.

When changing behavior that spans "the agent does X and the UI shows Y", you
need to add/extend both an `AgentCommand` variant and an `AiEvent` variant and
wire the handling on both ends.

### Provider abstraction (`ai/providers.rs`, `ai/stream.rs`)

Each LLM provider (Ollama, OpenAI, Anthropic, Groq; Gemini is stubbed/unsupported)
is wrapped into a type-erased `DynAgent` (a boxed streaming-chat closure, built
via the shared `dyn_agent_from!` macro so the stream-adaptation logic lives in
one place) so the rest of the code doesn't care which `rig` client backs the
current model. `openai`-type providers speak Chat Completions by default;
`api = "responses"` (on the provider, or overridden per `[[named_models]]`
entry via `NamedModel::api`) makes `build_openai` use rig's Responses client
instead, for models that reject `/chat/completions`. `resolve_agent` picks a
provider based on the config's named models, falling back to treating the
alias as a raw Ollama model name.
Streaming responses are normalized into `OurItem`/`OurStream` (`map_item`, in
`ai/stream.rs`) and then driven by `drive_stream`, which `tokio::select!`s
between the model stream and incoming `AgentCommand`s (so cancel/approve/deny
can interrupt an in-flight turn).

### Tool execution & approval (`approval.rs`, `tools.rs`, `tools/*.rs`)

Tools implement `rig::tool::Tool` (one file per tool under `src/tools/`,
re-exported from `tools.rs`). Each is registered on the `ToolServer` wrapped in
a `GatedTool` (approval.rs), which:

1. Checks `PermissionMode` (`auto` / `ask-dangerous` / `ask-always`) and the
   tool's `is_dangerous` flag to decide whether to pause for user approval
   (round-trips through the UI via `AiEvent::ToolCallApprovalRequired` and an
   `oneshot` channel registered in the `ApprovalGate` map).
2. Fires `pre_tool_call` / `post_tool_call` hooks around the actual call.

Danger is an `approval::Danger` (`Safe`, `Always`, or `Classify(closure over the args JSON)`
decided per call); `bool` converts into it. `write_file` and `edit_file` are
`Always`; `shell_command` is `Classify(shell_command::is_dangerous)`, a
fail-closed read-only allowlist (`ls`, `rg`, `git status/log/diff`, `cargo
check/test/…`) extended by the user's `[permissions] safe_commands` prefixes.
`[permissions] safe_tools` forces named tools (MCP included) to `Safe` in
`GateContext::wrap`. New tools should be registered in `build_tool_server`
(`ai.rs`) with an explicit danger.

`permission_mode = "smart"` sends dangerous calls to a reviewer model
(`[permissions.reviewer]`, a `BackgroundModelConfig` built by
`background::build_reviewer` before the tool server). The gate injects a
required `justification` argument into dangerous tools' schemas and strips it
before the real call; the reviewer's verdict (`approval::review::parse_review`)
allows, declines with a suggestion, asks the user (approval card shows
`review_note`), or blocks. A declined call resubmitted with identical args goes
to the user (`SmartReview::denied`, cleared on `/clear`). No reviewer, or no
usable verdict, falls back to asking the user — never to allowing.
The values a `GatedTool` needs (mode, gate map, event sender, hook runner,
outcome counter, smart reviewer, safe tools) are bundled as `approval::GateContext`; `ctx.wrap(tool,
dangerous)` is how both built-in and MCP tools get gated.
`grep_search` and `find_files` share their glob-pattern matching via
`tools::glob` (`glob_to_regex`/`glob_match`) rather than duplicating it.

### Extensibility: skills, plugins, hooks, MCP

- **Skills** (`skills.rs`) are markdown files (`.magai/skills/*.md` project-local,
  `~/.config/magai/skills/*.md` global) invoked as `/skill-name`; their content
  is rendered (with `{{args}}` substitution) and sent to the agent as a message.
- **Plugins** (`plugins.rs`) are directories with a `plugin.toml` manifest that
  can bundle an MCP server definition, skills, and hooks. Discovered from
  `.magai/plugins/` (project, takes precedence) and `~/.config/magai/plugins/`
  (global), merged by name.
- **Hooks** (`hooks.rs`) are shell commands fired fire-and-forget on lifecycle
  events (`session_start`, `session_stop`, `pre_tool_call`, `post_tool_call`,
  `agent_response`), with `MAGAI_*` env vars carrying context.
- **MCP servers** (`mcp.rs`) are reached over stdio (a spawned child process) or
  streamable HTTP (a remote `url`, with optional `bearer_token`/`headers`) —
  `McpServerConfig::transport` (`config.rs`) decides which, expanding `${VAR}`
  references from the environment. `GatedMcpHandler` replaces rig's
  `McpClientHandler` so each discovered tool is wrapped in a `GatedTool` before
  being registered on the shared `ToolServerHandle`: MCP tools are treated as
  dangerous unless the server is marked `trusted`, and a tool whose name is
  already taken is skipped rather than shadowing the existing one. Servers are
  connected after every built-in tool is registered so that check sees them all;
  connection failures surface as `AiEvent::Error` in the TUI. `/mcp` reports
  each configured server (transport, gating, live tool list) — the status lives
  with the agent task, so it round-trips as `AgentCommand::ListMcp` →
  `AiEvent::McpStatus`. A child server's stderr is piped (never inherited — it
  would scribble over the TUI) and its tail is appended to connection errors,
  and each connection is bounded by the server's `timeout_secs`.

- **Background helper models** (`ai/background.rs`) — the memory fact
  extractor (`[memory.extractor]`) and the quality judge (`[quality.judge]`)
  share `config::BackgroundModelConfig` (`model`, `prompt`/`prompt_file`,
  `timeout_secs`) and run as a `BackgroundModel` after each turn. Their
  prompts and reply parsing are pure functions next to what they store
  (`memory::extract::{DEFAULT_EXTRACTOR_PROMPT, parse_triples, store_facts}`,
  `memory::quality::{DEFAULT_JUDGE_PROMPT, judge_input, parse_verdict}`).
  Models resolve via `providers::resolve_agent_with_preamble`, so any
  `[[named_models]]` alias works, and that entry's `system_prompt` is
  deliberately ignored in favour of the helper's prompt. Both are built once
  in `run_agent` (`background::Helpers`), so a bad model is reported at
  startup rather than failing silently each turn. Never add a helper that
  talks to a provider directly or hardcodes a model — give it a
  `BackgroundModelConfig` instead.

Plugin-provided hooks/skills/MCP configs are merged with config-level ones in
`run_agent` — when touching one of these systems, check whether the plugin path
also needs updating.

### Config (`config.rs`)

Loaded from `~/.config/magai/config.toml` (TOML, see `docs/config.example.toml`
for the full annotated reference: providers, named models, default model,
permission mode, context window, MCP servers, hooks). `Config::load` returns
`(Config, Option<String>)`: missing config is silent, but unreadable/unparsable
config falls back to `Config::default()` *and* returns a warning string that
`main` forwards as an `AiEvent::Error` so it's visible in the TUI (stderr is
hidden behind the alternate screen).

### UI rendering (`ui/render.rs`, `ui/input.rs`, `ui/text.rs`)

`App::draw` (`ui/render.rs`) lays out title/separator/history/input regions
each frame and renders chat history as a flat `Vec<ChatMessage>` converted to
`Line`s by `build_lines`. Two wrapping paths exist and must both stay
overflow-safe: plain text (`word_wrap`, used for user/system/tool messages,
measured in UTF-8 byte length — a safe upper bound on terminal display width)
and markdown (`markdown_to_static_lines` → `wrap_styled_line`, used for
assistant messages, which preserves per-character styles across wrap points
and hard-breaks tokens wider than the available width). Both wrapping helpers
and `floor_char_boundary` (multibyte-safe truncation) live in `ui/text.rs` as
pure, tested free functions; `App::handle_events` and friends (scrolling,
slash-command dispatch, autocomplete, the approval-card key intercept, message
submission) live in `ui/input.rs`. `view_width`/`view_height` are recomputed
from the actual `history_area` each frame and drive both wrapping and
scroll-offset math — don't hardcode terminal dimensions.

Colours come from `App::theme` (`ui/theme.rs`), never `Color::*` literals in
render code. `Theme` is a flat set of semantic slots (`user`, `accent`,
`danger`, `diff_add`, …) and also implements `tui_markdown::StyleSheet`, so
assistant markdown follows it too. Built-ins are `catppuccin-mocha` (default)
and `classic` (the original palette); config `theme = "<name>"` picks one and
`[themes.<name>]` tables (`base` + per-slot overrides) are validated by
`Theme::resolve`, which warns rather than failing. `/theme` switches live.
A new colour role means a new field on `Theme` — set it in both built-ins and
add it to `slot_mut` so config can override it. Popups draw over `Clear`, which
resets the background, so their `Block`s re-apply `theme.base()`.

Up/Down prompt recall reads `App::input_history`, a `history::History` backed
by one global `$XDG_DATA_HOME/magai/history.jsonl` (not per project or
session). Entries are appended as JSON strings, one per line, so concurrent
instances don't clobber each other and multi-line prompts survive; the file is
compacted to the newest 1000 once it doubles. Record via `App::record_history`.

### System prompt / project instructions

The base system prompt lives in `src/ai/preamble.md` (pulled in via
`include_str!` as `ai::PREAMBLE`) — edit that file directly to change the
default agent persona/instructions, no Rust changes needed.
`ai::read_project_instructions` looks for `AGENTS.md` then `CLAUDE.md` in the
working directory and `build_preamble`/`build_preamble_from` append its
contents (under a "# Project instructions" heading) to the base preamble.

A `[[named_models]]` entry (`config.rs`) can override the base preamble for
just that model via `system_prompt` (inline) or `system_prompt_file` (path,
`system_prompt` wins if both are set) — `NamedModel::resolve_system_prompt`
reads it, and `providers::resolve_agent` swaps it in for `PREAMBLE` before
calling `build_preamble_from`; project instructions are still appended on
top either way. The default (non-override) preamble is still built once at
startup and threaded as a `&str` through `resolve_agent` and every `build_*`
provider constructor — when adding a new provider builder or a new way to
switch models (alongside `SetModel`/`UseProviderModel`), make sure it also
receives `&preamble` (and, if it should support per-model overrides,
`&project_ctx` plus a `find_named_model` lookup) so project instructions
stay in effect.

### Checkpoints (`checkpoint.rs`, `checkpoint/{parse,format}.rs`)

Per-turn snapshots of the working tree, kept in a **shadow git repository**
under `$XDG_DATA_HOME/magai/checkpoints/<slug>/git`. Every git call passes
`--git-dir=<shadow> --work-tree=<project>`, so the user's own repository is
never read or written — no commits, no index, no HEAD, no stash, no reflog, no
hooks, no signing. Checkpointing therefore works on a dirty tree, alongside the
user's own commits, and in directories that are not git repos at all. Because
snapshots capture the *tree* rather than tool calls, `shell_command` writes are
covered as well as `write_file`/`edit_file`.

`CheckpointStore::snapshot` runs `add -A` → `write-tree` → `commit-tree` →
`update-ref` on `refs/heads/magai`. Plumbing rather than `git commit`: it runs
no hooks, honours no `commit.gpgsign`, and needs no HEAD. A snapshot whose tree
matches the previous one is skipped, so a turn that changed nothing leaves no
row behind. Metadata rides in the commit message as `Magai-*` trailers (id,
kind, outcome, model, turn, session), which is why there is no sidecar file to
fall out of sync; ids are monotonic so pruning never renumbers survivors.

Snapshots are taken at session start, **after every turn whatever the
`DriveResult`** (`snapshot_turn` in `ai.rs` — `Done`-only was the bug that made
`/undo` revert the wrong turn after a Ctrl-C), and before every destructive
action so that action is itself reversible.

`/undo` is surgical: it reverse-applies just that turn's patch via
`git apply --reverse --check` then `--reverse`. Files the user edited during the
turn are untouched, and a genuine collision fails the `--check` so nothing is
written. `--3way` is deliberately not used — it implies `--index` and leaves
conflict markers instead of refusing. `/restore` is `read-tree -u --reset` plus
`clean -fdq`, never `reset --hard`, which would move the branch and break the
append-only log that lets `/redo` survive a `/restore`.

Two traps encoded in the code: `slug_for` hand-rolls FNV-1a because
`DefaultHasher` is not stable across Rust releases (a changed hash orphans every
existing checkpoint), and `CheckpointStore::command` scrubs `GIT_DIR` and
friends from the environment so magai launched from a git hook cannot have its
snapshots redirected into the user's repo.

The shadow's `info/attributes` (`* -text -filter …`) is load-bearing: without
`-filter`, snapshots in a git-lfs project store pointers and a restore smudges
garbage into the working tree.
