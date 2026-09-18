# magai

A terminal-based AI coding agent — like having Claude Code or a similar
assistant living in your terminal, but provider-agnostic, scriptable, and
yours to configure.

## Why magai?

- **Run any model.** Point it at a local Ollama model, OpenAI, Anthropic,
  Groq, or an OpenAI-compatible proxy, and switch between them mid-session
  with `/model`.
- **It can actually do things.** magai gives the agent tools to read, write,
  and edit files, search and grep your codebase, run shell commands, inspect
  git status/diffs, and fetch web pages — so it can work on your project, not
  just talk about it.
- **You stay in control.** An approval gate prompts before dangerous actions
  (writing files, editing files, running shell commands) — configurable from
  "ask for everything" to "just do it."
- **A safety net is built in.** magai snapshots your working tree after every
  turn, so a bad agent edit is always one `/undo` away — and `/diff` shows you
  exactly what changed. Snapshots live outside your repository, so magai never
  commits, stashes, or stages anything on your behalf.
- **Extensible.** Add custom `/slash-command` skills, hooks that fire shell
  commands on lifecycle events (session start/stop, tool calls, responses),
  plugins that bundle skills + hooks + an MCP server together, and connect to
  any MCP server for more tools.

## Quick start

### 1. Build it

```sh
cargo build --release
```

The binary is at `target/release/magai`.

### 2. Get a model running

The simplest path is [Ollama](https://ollama.com) — no API key needed:

```sh
ollama pull granite4:latest
```

magai will use this as the default if you don't configure anything else.

To use a hosted provider instead (OpenAI, Anthropic, Groq), export the
relevant API key as an environment variable, e.g.:

```sh
export ANTHROPIC_API_KEY=sk-...
```

### 3. (Optional) configure

magai works out of the box with no config file. To define named models,
change the default model, set a permission mode, register MCP servers, or add
hooks, copy the annotated example to `~/.config/magai/config.toml`:

```sh
mkdir -p ~/.config/magai
cp docs/config.example.toml ~/.config/magai/config.toml
```

Then edit it — the file documents every option inline (models, default model,
permission mode, context window size, MCP servers, hooks).

### 4. Run it

```sh
cargo run --release
# or, once built:
./target/release/magai
```

Type a message and press Enter to chat. Use `/help` inside the app to see
available slash commands (`/model`, `/tools`, `/clear`, `/undo`, `/plugins`,
…), and `/model <alias>` to switch models on the fly.

## Permission modes

Control how often magai pauses to ask before running a tool, via
`permission_mode` in the config:

- `ask-dangerous` (default) — prompt only before writing/editing files or
  running shell commands.
- `ask-always` — prompt before every tool call.
- `auto` — never prompt; tools run immediately.

## Extending magai

- **Skills** — drop a markdown file in `.magai/skills/` (project) or
  `~/.config/magai/skills/` (global) and invoke it as `/<filename>`.
- **Plugins** — a directory with a `plugin.toml` manifest in
  `.magai/plugins/` or `~/.config/magai/plugins/` can bundle an MCP server,
  skills, and hooks together. Run `/plugins` to see what's loaded.
- **Hooks** — shell commands triggered on `session_start`, `session_stop`,
  `pre_tool_call`, `post_tool_call`, or `agent_response`, configured in
  `config.toml` (see `docs/config.example.toml` for examples and the
  `MAGAI_*` environment variables passed to them).
- **Project instructions** — drop an `AGENTS.md` or `CLAUDE.md` in your
  project root and its contents are appended to the agent's system prompt at
  startup, so you can give it project-specific context and conventions.

See `docs/config.example.toml` for the full configuration reference.
