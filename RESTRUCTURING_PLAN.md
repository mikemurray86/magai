# magai — Maintainability & Clarity Restructuring

## Context

`magai` is a ~6,150-line Rust terminal coding-agent TUI. At the macro level it is
already well-architected: a clean channel-split between `ai::run_agent` and
`ui::App`, one-file-per-tool under `src/tools/`, and a set of small focused
modules (`config`, `hooks`, `skills`, `plugins`, `mcp`, `approval`,
`slash_commands`) that are each in good shape.

The strain is concentrated in two files plus some scattered duplication and
error-handling gaps:

- **`src/ui.rs` (1297 lines)** mixes rendering (`draw` ~300 lines), input/event
  handling (`handle_events` ~270 lines), text-wrapping/markdown helpers
  (~200 lines), and autocomplete state — all on one god `App` struct (24 fields).
- **`src/ai.rs` (604 lines)** has three near-identical provider builders with a
  100%-duplicated stream closure (`build_ollama`/`build_openai`/`build_anthropic`,
  lines 103–167), per-provider model discovery, a 70-line `drive_stream`, and a
  164-line `run_agent` command loop (lines 491–598).
- **Duplication:** `glob_to_regex` + `glob_match` are copied verbatim in
  `tools/grep_search.rs:131-157` and `tools/find_files.rs:100-125`; popup-render
  loops and `make_textarea`/`make_textarea_with` repeat in `ui.rs`.
- **Robustness gaps:** swallowed errors in `git_checkpoint` (ai.rs:410), hook
  spawn (hooks.rs), the `ollama::Client::new(Nothing).unwrap()` panic
  (ai.rs:104), and config-parse errors that `eprintln!` to a hidden stderr.
- **No tests exist.**

Goal: improve long-term maintainability and clarity **without changing
behavior** (except making silent failures visible), and seed a test suite for
the pure logic. Work is sequenced as independently-committable, compiling steps.

## Guiding principles

- **No behavior change** except surfacing previously-swallowed errors.
- Every step ends with `cargo build && cargo clippy && cargo fmt` clean and is
  committed on its own.
- Prefer converting a flat module (`foo.rs`) to a directory (`foo/mod.rs` +
  submodules) so the public surface (`crate::ai::run_agent`, `crate::ui::App`,
  `AgentCommand`, `AiEvent`) stays identical and `main.rs` is untouched.
- Keep `CLAUDE.md` in sync — its Architecture section references these files.

---

## Step 1 — Extract shared helpers / dedup (lowest risk)

**1a. Shared glob matching.** Create `src/tools/glob.rs` with one
`pub(crate) fn glob_to_regex(&str) -> String` and `pub(crate) fn glob_match(pattern, name) -> bool`
(identical bodies already exist). Add `mod glob;` to `tools.rs`. Replace the
copies in `tools/grep_search.rs` and `tools/find_files.rs` with
`use crate::tools::glob::glob_match;`.

**1b. Provider stream closure.** In `ai.rs`, the `DynAgent(Box::new(move |msg,hist| …))`
body is identical across all three builders. Extract a helper that takes the
built `Arc<Agent>` and returns the `DynAgent`, so each builder ends with
`Ok(dyn_agent_from(agent))`. Removes ~30 duplicated lines. (This lands in Step 2's
`ai/providers.rs` if Step 2 is done together, but can be done in place first.)

**1c. Textarea constructor.** Collapse `make_textarea` / `make_textarea_with`
(ui.rs:1069-1093) into one `make_textarea(text: &str, waiting: bool)` and update
the two call sites; the placeholder/style block is currently duplicated verbatim.

Verify: `cargo test` (glob tests added in Step 4 will exercise 1a), `cargo build`.

---

## Step 2 — Split `ai.rs` into an `ai/` module

Convert `src/ai.rs` → `src/ai/mod.rs` keeping `pub use`/`pub` items
(`run_agent`, `AgentCommand`, `DEFAULT_MODEL`) so callers are unchanged. Move
internals into focused submodules:

- **`ai/providers.rs`** — `DynAgent`, `dyn_agent_from` (from 1b), `build_ollama`/
  `build_openai`/`build_anthropic`, `resolve_agent`, `api_key_from_env`, and the
  `fetch_*_models` / `fetch_models` discovery fns (ai.rs:103-262).
- **`ai/stream.rs`** — `OurItem`, `OurStream`, `map_item`, `DriveResult`,
  `drive_stream` (ai.rs:39-87, 264-340).
- **`ai/mod.rs`** — keeps `run_agent`, the preamble/`PREAMBLE`/`build_preamble`,
  `build_tool_server`, and git/history helpers (`git_checkpoint`, `git_undo`,
  `is_git_repo`, `trim_history`, `read_project_instructions`).

Optional clarity win inside `run_agent`: `SetModel` and `UseProviderModel` both
rebuild the agent — factor the rebuild into a small closure/fn to reduce the
164-line loop. Keep the loop's behavior identical.

Verify: `cargo build` (the only true check that the module split is wired right).

---

## Step 3 — Split `ui.rs` into a `ui/` module

Convert `src/ui.rs` → `src/ui/mod.rs` keeping `pub struct App`, `pub enum AiEvent`,
and `App::{new,run}` public. Move cohesive blocks into submodules; `App` stays
the owner but its `impl` is spread across files via `impl App { … }` blocks:

- **`ui/text.rs`** — pure formatting, the most testable code:
  `markdown_to_static_lines`, `wrap_styled_line`, `chars_to_line`,
  `floor_char_boundary`, `word_wrap`, `format_tool_call`, `summarize_result`
  (ui.rs:1096-1286). No `self` dependency — move as free functions.
- **`ui/render.rs`** — `App::draw`, `App::build_lines`, and the three popup/card
  render sections. Consider one private `draw_popup_list` helper to collapse the
  repeated enumerate-and-style loops.
- **`ui/input.rs`** — `App::handle_events`, `submit_input`, `handle_slash_command`,
  `push_system`. Break `handle_events` into `handle_mouse`, `handle_key`, and an
  approval-intercept helper.
- **`ui/mod.rs`** — `App` struct + `ChatMessage`/`Role`/`PendingApproval`/`AiEvent`
  types, `App::new`, `App::run`, `poll_ai_events`, autocomplete candidate fns,
  `make_textarea`.

Note on the god struct: full field-grouping into `RenderState`/`AutocompleteState`
sub-structs is **out of scope** (touches every method, high churn for low gain);
the file split alone resolves the maintainability concern. Document the field
groups with comments instead.

Verify: `cargo build`, then `cargo run` and manually exercise scroll, slash
commands, `/model` autocomplete, and an approval prompt to confirm no regression
(see Verification).

---

## Step 4 — Robustness fixes

- **`ai.rs` ollama unwrap (line 104):** make `build_ollama` return
  `Result<DynAgent, String>` like the others (or map the error), so a failed
  client construction surfaces via `AiEvent::Error` instead of panicking. Update
  `resolve_agent` and the two `build_ollama` call sites in `run_agent`.
- **`git_checkpoint` (ai.rs:410):** capture the `Command::output()` result; on
  non-success send a non-fatal `AiEvent::Error`/notice so a failed stash isn't
  silent (it means `/undo` won't work).
- **Hook spawn (hooks.rs):** on spawn error, `tracing::warn!` instead of
  `let _ = …` so failed hooks are diagnosable.
- **Config parse (config.rs):** the current `eprintln!` is invisible under the
  TUI. Return the parse error up to `main`/startup so it can be shown as a system
  message, or at minimum route it through `tracing` to the log file. Keep the
  fall-back-to-default behavior.

Verify: `cargo build`; temporarily break a config to confirm the error is now
visible; confirm normal startup unaffected.

---

## Step 5 — Seed the test suite

Add `#[cfg(test)] mod tests` to the pure, dependency-free units:

- **`tools/glob.rs`** — `glob_to_regex`/`glob_match`: `*.rs`, `**/`, literal,
  `?`, no-match cases.
- **`slash_commands.rs`** — command matching/autocomplete and dispatch to
  `SlashCommandAction` variants.
- **`config.rs`** — parse a representative TOML (from `docs/config.example.toml`)
  and assert provider/model/permission fields; assert malformed TOML yields the
  error path (post-Step-4).
- **`ui/text.rs`** — `word_wrap` (width boundaries, long unbreakable tokens),
  `floor_char_boundary` (multibyte safety), and a `wrap_styled_line` width case.

Verify: `cargo test` green.

---

## Step 6 — Docs sync

- Add module-level `//!` doc comments to each new submodule stating its single
  responsibility (the `DynAgent`/`OurItem`/`OurStream` abstractions especially —
  currently undocumented).
- Update `CLAUDE.md` Architecture section so the file references match the new
  `ai/` and `ui/` layouts and mention the test modules and `tools/glob.rs`.

---

## Files touched (summary)

- New: `src/tools/glob.rs`, `src/ai/{mod,providers,stream}.rs`,
  `src/ui/{mod,render,input,text}.rs`.
- Removed (replaced by dir modules): `src/ai.rs`, `src/ui.rs`.
- Edited: `src/tools.rs` (`mod glob;`), `tools/grep_search.rs`,
  `tools/find_files.rs`, `src/hooks.rs`, `src/config.rs`, `CLAUDE.md`.
- Untouched public surface: `main.rs`, all `AgentCommand`/`AiEvent` variants,
  every tool's behavior.

## Verification (end-to-end)

1. `cargo build && cargo clippy && cargo fmt --check` clean after each step.
2. `cargo test` — new unit tests green.
3. `cargo run` smoke test (no behavior change expected):
   - Send a message, confirm streaming tokens render.
   - Trigger a tool call requiring approval (e.g. a write) → approval card
     appears, approve/deny works.
   - `/model` → autocomplete popup, selection switches model.
   - Scroll history (mouse + PageUp/Down), `/clear`, `/undo`.
4. Robustness checks: break config TOML → error now visible; in a non-git dir
   confirm checkpoint notice path doesn't fire spuriously.

## Out of scope (deliberately)

- Splitting `App` fields into sub-structs (high churn, low payoff).
- Splitting `AiEvent`/`AgentCommand` into nested enums (current size is fine).
- Adding Gemini/Groq provider support, regex caching, or token-count accuracy.
- Integration/TUI snapshot tests (would need a ratatui capture harness).
