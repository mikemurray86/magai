# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`magai` is a terminal coding-agent application — a TUI (built on `ratatui`/`crossterm`)
that drives an AI agent (via `rig-core`) capable of calling tools (file
read/write/edit, shell, git, grep, web fetch, MCP servers) against the local
project, with an approval gate for dangerous operations. Conceptually it's a
Claude-Code-like agent harness, written in Rust.

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

There is no README, lint config, or CI in the repo currently — don't assume
conventions beyond what's in the source.

## Architecture

### Two async halves connected by channels

`main.rs` spawns two long-lived tasks that never call each other directly —
they communicate purely through `tokio::mpsc` channels:

- **`ai::run_agent`** (`src/ai/mod.rs`, plus `ai/providers.rs` and
  `ai/stream.rs`) — owns the LLM agent, conversation history, tool server, and
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
current model. `resolve_agent` picks a provider based on the config's named
models, falling back to treating the alias as a raw Ollama model name.
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

`write_file`, `edit_file`, and `shell_command` are marked dangerous; everything
else (reads, search, git status/diff, web fetch) is not. New tools should be
registered in `build_tool_server` (`ai/mod.rs`) with an explicit danger flag.
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
- **MCP servers** (`mcp.rs`) are spawned as child processes and connected via
  `rmcp`, exposing their tools through the same `ToolServerHandle` as built-in
  tools.

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

### Project instructions

`ai::read_project_instructions` looks for `AGENTS.md` then `CLAUDE.md` in the
working directory and `build_preamble` appends its contents (under a
"# Project instructions" heading) to the base `PREAMBLE` once at startup. The
combined preamble is threaded as a `&str` through `resolve_agent` and every
`build_*` provider constructor — when adding a new provider builder or a new
way to switch models (alongside `SetModel`/`UseProviderModel`), make sure it
also receives `&preamble` so project instructions stay in effect.

### Git safety net

If the working directory is a git repo, `git_checkpoint` stashes
(`git stash push --include-untracked`) before each agent turn, and `/undo`
runs `git stash pop` — this is how the agent's file edits can be reverted.
