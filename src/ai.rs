//! Owns the LLM agent loop: builds the tool server, resolves/rebuilds the
//! active provider agent, drives streaming turns, and bridges to the UI via
//! `AgentCommand`/`AiEvent` channels. See `providers` for provider/model
//! resolution and `stream` for stream normalization and driving.

mod providers;
mod stream;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rig::message::Message;
use rig::tool::server::{ToolServer, ToolServerHandle};
use tokio::sync::mpsc;

use crate::approval::{ApprovalGate, GateContext, ToolOutcomeCounter};
use crate::checkpoint::{CheckpointStore, SnapshotMeta};
use crate::config::{Config, ProviderType, StartupChoice};
use crate::hooks::{HookEvent, HookRunner};
use crate::memory::{default_db_path, MemoryDb, ToolCallRecord};
use crate::ui::AiEvent;

use providers::{
    api_key_from_env, build_startup_agent, fetch_models, fetch_models_for, fetch_ollama_models,
    gemini_unsupported, ollama_base_url, resolve_agent, DynAgent,
};
use stream::{drive_stream, DriveResult};

/// How to rebuild the current agent when only the tool server changes.
/// Deliberately separate from the `current_model` display string: after
/// `/provider` that string is `provider/model`, which matches no
/// `[[named_models]]` alias and used to fall through to Ollama.
enum ModelSelection {
    /// A `[[named_models]]` alias, re-resolved so its `system_prompt` override
    /// keeps applying.
    Alias(String),
    Direct(crate::config::StartupModel),
}

/// Decides — and validates — which model magai will start with, before the
/// TUI takes over the terminal. Errors are returned so `main` can print them
/// to stderr and exit, rather than opening an alternate screen that hides
/// them behind a session that fails on every turn.
pub async fn preflight_startup(config: &Config) -> Result<crate::config::StartupModel, String> {
    match config.startup_choice(&|k| std::env::var(k).ok())? {
        StartupChoice::Model(startup) => validate_model(config, startup).await,
        // A provider is reachable but nothing named a model. Report what it
        // actually offers rather than guessing an id: a guess goes stale, may
        // not exist on the account, and can silently land on a model that is
        // not even a chat model.
        StartupChoice::NeedsModel(unresolved) => {
            let models = fetch_models_for(&unresolved.provider, config)
                .await
                .map_err(|e| lookup_failed_message(&unresolved, &e))?;
            Err(needs_model_message(&unresolved, &models))
        }
    }
}

/// Confirms a chosen model can actually be used, before the TUI hides stderr.
async fn validate_model(
    config: &Config,
    startup: crate::config::StartupModel,
) -> Result<crate::config::StartupModel, String> {
    match startup.provider_type {
        ProviderType::Gemini => return Err(gemini_unsupported()),
        // A key that is missing now would only surface as an auth failure on
        // the first turn, so check it here. No network call.
        ProviderType::OpenAI | ProviderType::Anthropic | ProviderType::Groq => {
            api_key_from_env(startup.api_key_env.as_deref()).map_err(|e| {
                format!(
                    "model {:?} cannot be used: {e}. Export the key, or set \
                     default_model to a model whose credentials are available.",
                    startup.display
                )
            })?;
        }
        ProviderType::Ollama => {
            let base = ollama_base_url(config, startup.base_url.as_deref());
            let tags = fetch_ollama_models(&base)
                .await
                .map_err(|e| ollama_unreachable_message(&base, &e))?;
            if let Some(warning) = reconcile_ollama_model(&startup, &tags, &base)? {
                eprintln!("warning: {warning}");
            }
        }
    }
    Ok(startup)
}

/// How many catalogue entries to print before summarising the rest. Enough to
/// recognise a model, short enough to stay readable against an aggregator that
/// lists several hundred.
const MODEL_LIST_LIMIT: usize = 20;

fn needs_model_message(
    unresolved: &crate::config::UnresolvedProvider,
    models: &[String],
) -> String {
    let label = &unresolved.label;
    // Hosted catalogues carry a creation date and are sorted by it; Ollama
    // tags are just what is installed locally, so claim no order for them.
    let ordering = match unresolved.provider.provider_type {
        ProviderType::Ollama => "installed",
        _ => "available, newest first",
    };
    if models.is_empty() {
        return format!(
            "{}, and provider {label:?} reports no models to choose from.\n  \
             Check the account, or configure a different provider.",
            unresolved.detected_via
        );
    }
    let shown: Vec<String> = models
        .iter()
        .take(MODEL_LIST_LIMIT)
        .map(|m| format!("    {m}"))
        .collect();
    let rest = models.len().saturating_sub(shown.len());
    let more = if rest > 0 {
        format!("\n    ... {rest} more")
    } else {
        String::new()
    };
    format!(
        "{}, but no model is chosen and magai will not guess one.\n\n  \
         Models from {label:?} ({} {ordering}):\n{}{more}\n\n  \
         Set default_model in ~/.config/magai/config.toml, or add a \
         [[named_models]] entry.",
        unresolved.detected_via,
        models.len(),
        shown.join("\n"),
    )
}

fn lookup_failed_message(unresolved: &crate::config::UnresolvedProvider, err: &str) -> String {
    format!(
        "{}, but magai could not list its models to choose from: {err}\n  \
         Set default_model in ~/.config/magai/config.toml to name a model \
         directly, or fix access to provider {:?}.",
        unresolved.detected_via, unresolved.label
    )
}

/// Reconciles the chosen Ollama model against the tags the server actually
/// holds. The choice is always kept — magai never substitutes a different
/// model — but a tag the server does not list earns a warning, since the user
/// may simply be about to pull it. Returns the warning text, if any.
fn reconcile_ollama_model(
    startup: &crate::config::StartupModel,
    tags: &[String],
    base: &str,
) -> Result<Option<String>, String> {
    if tags.is_empty() {
        return Err(format!(
            "ollama at {base} has no models — run `ollama pull <model>`, \
             or configure a hosted provider."
        ));
    }
    if tags.contains(&startup.model) {
        return Ok(None);
    }
    Ok(Some(format!(
        "ollama at {base} does not list {:?} (has: {}). Continuing anyway — \
         run `ollama pull {}` if turns fail.",
        startup.model,
        tags.join(", "),
        startup.model
    )))
}

fn ollama_unreachable_message(base: &str, err: &str) -> String {
    format!(
        "the configured model needs ollama, but it is not reachable ({err}).\n  \
         Start it with `ollama serve`, point magai at another instance with \
         OLLAMA_API_BASE_URL or [providers.ollama].base_url in \
         ~/.config/magai/config.toml,\n  \
         or use a hosted provider by exporting ANTHROPIC_API_KEY, \
         OPENAI_API_KEY or GROQ_API_KEY.\n  Tried: {base}"
    )
}

/// The default system preamble, kept in its own file so it's easy to read
/// and tweak without wading through `ai.rs`. Per-model overrides (see
/// `NamedModel::system_prompt`/`system_prompt_file` in `config.rs`) replace
/// this text entirely rather than appending to it.
const PREAMBLE: &str = include_str!("ai/preamble.md");

pub enum AgentCommand {
    Message(String),
    SetModel(String),
    SetTools(bool),
    ApproveToolCall(String),
    DenyToolCall(String),
    Cancel,
    Clear,
    /// Work out what a checkpoint action would do, so the UI can confirm it
    /// before anything is written.
    CheckpointPreview(crate::checkpoint::CheckpointAction),
    /// Carry out a previously previewed checkpoint action.
    CheckpointApply(crate::checkpoint::CheckpointAction),
    /// Report the checkpoint list back as `AiEvent::CheckpointList`.
    CheckpointList,
    /// Report one checkpoint's diff back as `AiEvent::CheckpointDiff`.
    CheckpointDiff(Option<u64>),
    /// Response to `AiEvent::MaxTurnsReached`: `Some(n)` continues the
    /// paused turn for `n` more turns, `None` declines and ends the turn.
    MaxTurnsResponse(Option<usize>),
    ListModels(String),
    /// Request the `/mcp` status report for the configured servers.
    ListMcp,
    UseProviderModel {
        provider_alias: String,
        model_id: String,
    },
}

/// Combines the base system preamble with project-specific instructions
/// from `AGENTS.md`/`CLAUDE.md`, if present.
fn build_preamble(project_ctx: &str) -> String {
    build_preamble_from(PREAMBLE, project_ctx)
}

/// Same as `build_preamble`, but with an explicit base preamble rather than
/// the global default — used by `resolve_agent` when a named model supplies
/// its own `system_prompt`/`system_prompt_file`.
pub(crate) fn build_preamble_from(base: &str, project_ctx: &str) -> String {
    if project_ctx.trim().is_empty() {
        base.to_string()
    } else {
        format!("{base}\n\n# Project instructions\n\n{project_ctx}")
    }
}

// ── tool server ───────────────────────────────────────────────────────────────

async fn build_tool_server(ctx: &GateContext) -> ToolServerHandle {
    let handle = ToolServer::new().run();

    macro_rules! add {
        ($tool:expr, $dangerous:expr) => {
            handle.add_tool(ctx.wrap($tool, $dangerous)).await.ok();
        };
    }

    add!(crate::tools::ReadFile, false);
    add!(crate::tools::ReadFileRange, false);
    add!(crate::tools::WriteFile, true);
    add!(crate::tools::EditFile, true);
    add!(crate::tools::ListDirectory, false);
    add!(crate::tools::GrepSearch, false);
    add!(crate::tools::FindFiles, false);
    add!(crate::tools::GitStatus, false);
    add!(crate::tools::GitDiff, false);
    add!(crate::tools::WebFetch, false);
    add!(crate::tools::ShellCmd, true);

    handle
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn read_project_instructions() -> String {
    for name in ["AGENTS.md", "CLAUDE.md"] {
        if let Ok(s) = std::fs::read_to_string(name) {
            return s;
        }
    }
    String::new()
}

fn trim_history(history: &mut Vec<Message>, max_tokens: usize) -> usize {
    // Rough estimate: 4 chars ≈ 1 token
    let estimate_tokens = |msgs: &[Message]| -> usize {
        msgs.iter()
            .map(|m| serde_json::to_string(m).unwrap_or_default().len() / 4)
            .sum()
    };

    let mut dropped = 0;
    while history.len() > 2 && estimate_tokens(history) > max_tokens {
        history.remove(0);
        dropped += 1;
    }
    dropped
}

fn extract_final_assistant_text(history: &[Message]) -> String {
    use rig::message::AssistantContent;
    history
        .iter()
        .rev()
        .find_map(|msg| {
            if let Message::Assistant { content, .. } = msg {
                let text: String = content
                    .iter()
                    .filter_map(|c| match c {
                        AssistantContent::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                (!text.is_empty()).then_some(text)
            } else {
                None
            }
        })
        .unwrap_or_default()
}

/// Finds the first user-authored text in `messages` — the original prompt for
/// a turn, as opposed to any `UserContent::ToolResult` messages that follow
/// tool calls within the same turn.
fn extract_user_text(messages: &[Message]) -> String {
    use rig::message::UserContent;
    messages
        .iter()
        .find_map(|msg| {
            if let Message::User { content, .. } = msg {
                let text: String = content
                    .iter()
                    .filter_map(|c| match c {
                        UserContent::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                (!text.is_empty()).then_some(text)
            } else {
                None
            }
        })
        .unwrap_or_default()
}

fn epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Sends `ResponseStart`, streams `prompt` against `history` (sent as a
/// clone so `*history` still holds the pre-turn conversation if the turn is
/// cancelled; only overwritten in place by `OurItem::History`/`MaxTurnsReached`
/// handling inside `drive_stream` once the turn actually completes), and
/// returns the outcome plus whatever tool calls were recorded. Shared by the
/// fresh-message path and the max-turns continuation path so both get
/// identical stream-driving.
#[allow(clippy::too_many_arguments)]
async fn drive_turn(
    agent: &DynAgent,
    prompt: impl Into<Message>,
    history: &mut Vec<Message>,
    ai_tx: &mpsc::UnboundedSender<AiEvent>,
    user_rx: &mut mpsc::UnboundedReceiver<AgentCommand>,
    gate: &ApprovalGate,
    tool_timings: &mut HashMap<String, Instant>,
    current_model: &str,
    max_turns: usize,
) -> (DriveResult, Vec<ToolCallRecord>) {
    ai_tx
        .send(AiEvent::ResponseStart(current_model.to_string()))
        .ok();
    let stream = agent.stream_chat(prompt, history.clone(), max_turns).await;
    let mut tool_records: Vec<ToolCallRecord> = Vec::new();
    let result = drive_stream(
        stream,
        ai_tx,
        user_rx,
        history,
        gate,
        tool_timings,
        &mut tool_records,
    )
    .await;
    (result, tool_records)
}

/// Snapshots the working tree after a turn. Called for **every** outcome, not
/// just `Done`: a cancelled or max-turns turn still changed files, and the old
/// implementation's `Done`-only checkpoint is why `/undo` used to revert the
/// wrong turn after a Ctrl-C.
async fn snapshot_turn(
    store: &Option<Arc<CheckpointStore>>,
    ai_tx: &mpsc::UnboundedSender<AiEvent>,
    outcome: &str,
    label: &str,
    model: &str,
    turn_seq: usize,
) {
    let Some(store) = store else { return };
    let meta = SnapshotMeta::turn(label, model, turn_seq, outcome);
    let s = Arc::clone(store);
    match tokio::task::spawn_blocking(move || s.snapshot(&meta)).await {
        // A turn that changed no files leaves no checkpoint behind.
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            ai_tx
                .send(AiEvent::Error(format!("checkpoint failed: {e}")))
                .ok();
        }
        Err(e) => {
            ai_tx
                .send(AiEvent::Error(format!("checkpoint task failed: {e}")))
                .ok();
        }
    }
}

/// Labels a finished turn's checkpoint. Read before `finish_turn` consumes
/// the `DriveResult`.
fn drive_outcome(result: &DriveResult) -> &'static str {
    match result {
        DriveResult::Done => "done",
        DriveResult::Cancelled => "cancelled",
        DriveResult::MaxTurnsReached { .. } => "max_turns",
    }
}

/// Title for the confirmation card.
fn describe_action(
    action: crate::checkpoint::CheckpointAction,
    target: &crate::checkpoint::Checkpoint,
) -> String {
    use crate::checkpoint::CheckpointAction;
    match action {
        CheckpointAction::Undo => format!("undo #{} — {}", target.id, target.label),
        CheckpointAction::Redo => format!("redo #{} — {}", target.id, target.label),
        CheckpointAction::Restore(_) => format!(
            "restore the whole tree to #{} — {}",
            target.id, target.label
        ),
    }
}

/// Post-turn bookkeeping shared by every path that finishes driving a turn:
/// memory extraction, and — depending on outcome — either the `AgentResponse`
/// hook (`Done`) or stashing the resume prompt for the next
/// `MaxTurnsResponse` (`MaxTurnsReached`). Snapshots are taken separately by
/// `snapshot_turn`, which must fire for every outcome rather than just `Done`.
#[allow(clippy::too_many_arguments)]
fn finish_turn(
    result: DriveResult,
    tool_records: &[ToolCallRecord],
    history: &[Message],
    turn_start_idx: usize,
    turn_started_at: i64,
    pending_resume: &mut Option<Message>,
    memory_db: &Option<Arc<MemoryDb>>,
    config: &Config,
    current_model: &str,
    ai_tx: &mpsc::UnboundedSender<AiEvent>,
    hook_runner: &HookRunner,
    tool_outcomes: &ToolOutcomeCounter,
    session_id: &Option<String>,
    turn_seq: &mut usize,
) {
    // Counted for every turn, not just recorded ones: checkpoint labels read
    // it whether or not quality tracking is enabled.
    *turn_seq += 1;

    if let Some(db) = memory_db {
        crate::memory::extract::process_turn(db, current_model, tool_records);

        // LLM-based fact extraction — fire-and-forget, opt-in via config.
        if let Some(model) = config.memory.extract_facts_model.clone() {
            let text = extract_final_assistant_text(history);
            if !text.is_empty() {
                let db = Arc::clone(db);
                let alias = current_model.to_string();
                let base_url = config.ollama_base_url();
                tokio::spawn(crate::memory::extract::extract_facts_async(
                    db, text, model, alias, base_url,
                ));
            }
        }

        // Session/turn transcript + quality tracking — opt-in via config.
        if config.quality.enabled {
            if let Some(session_id) = session_id {
                let outcome = match &result {
                    DriveResult::Done => "done",
                    DriveResult::MaxTurnsReached { .. } => "max_turns_reached",
                    DriveResult::Cancelled => "cancelled",
                };
                let turn_slice = &history[turn_start_idx.min(history.len())..];
                let user_text = extract_user_text(turn_slice);
                let assistant_text = extract_final_assistant_text(turn_slice);
                let stats = std::mem::take(&mut *tool_outcomes.lock().unwrap());

                let turn_id = crate::memory::quality::record_turn(
                    db,
                    session_id,
                    *turn_seq,
                    current_model,
                    outcome,
                    &user_text,
                    &assistant_text,
                    tool_records,
                    stats,
                    turn_started_at,
                    epoch_secs(),
                );
                ai_tx
                    .send(AiEvent::TurnRecorded {
                        turn_id: turn_id.clone(),
                    })
                    .ok();

                if outcome == "done" && !assistant_text.is_empty() {
                    if let Some(judge_model) = config.quality.judge_model.clone() {
                        let db = Arc::clone(db);
                        let base_url = config.ollama_base_url();
                        tokio::spawn(crate::memory::quality::judge_turn_async(
                            db,
                            turn_id,
                            judge_model,
                            user_text,
                            assistant_text,
                            base_url,
                        ));
                    }
                }
            }
        }
    }

    match result {
        DriveResult::Done => {
            hook_runner.fire(
                HookEvent::AgentResponse,
                HashMap::from([("MAGAI_MODEL".to_string(), current_model.to_string())]),
            );
        }
        DriveResult::MaxTurnsReached { pending_prompt } => {
            *pending_resume = Some(*pending_prompt);
        }
        DriveResult::Cancelled => {}
    }
}

// ── agent task ────────────────────────────────────────────────────────────────

pub async fn run_agent(
    mut user_rx: mpsc::UnboundedReceiver<AgentCommand>,
    ai_tx: mpsc::UnboundedSender<AiEvent>,
    config: Config,
    startup: crate::config::StartupModel,
) {
    // Discover plugins and merge their resources with config-level resources
    let plugins = crate::plugins::discover();
    let plugin_hooks = crate::plugins::extract_hooks(&plugins);
    let plugin_mcp_configs = crate::plugins::extract_mcp_configs(&plugins);

    let all_hooks: Vec<_> = config
        .hooks
        .iter()
        .chain(plugin_hooks.iter())
        .cloned()
        .collect();
    let hook_runner = Arc::new(HookRunner::new(all_hooks));

    let gate: ApprovalGate = Arc::new(Mutex::new(HashMap::new()));
    let tool_outcomes: ToolOutcomeCounter = Arc::new(Mutex::new((0, 0, 0)));
    let gate_ctx = GateContext {
        mode: config.permission_mode,
        gate: gate.clone(),
        event_tx: ai_tx.clone(),
        hook_runner: hook_runner.clone(),
        tool_outcomes: tool_outcomes.clone(),
    };
    let tool_handle = build_tool_server(&gate_ctx).await;

    // ── memory ────────────────────────────────────────────────────────────────
    let memory_db: Option<Arc<MemoryDb>> = if config.memory.enabled {
        let path = config
            .memory
            .db_path
            .as_ref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(default_db_path);
        match MemoryDb::open(&path) {
            Ok(db) => {
                let db = Arc::new(db);
                tool_handle
                    .add_tool(gate_ctx.wrap(crate::tools::MemoryQuery::new(db.clone()), false))
                    .await
                    .ok();
                tool_handle
                    .add_tool(gate_ctx.wrap(
                        crate::tools::MemorySave::new(db.clone(), startup.display.clone()),
                        false,
                    ))
                    .await
                    .ok();
                Some(db)
            }
            Err(e) => {
                ai_tx
                    .send(AiEvent::Error(format!("memory: could not open db: {e}")))
                    .ok();
                None
            }
        }
    } else {
        None
    };

    // ── MCP ───────────────────────────────────────────────────────────────────
    // Connected last, so a server's tools can be checked against every
    // built-in name (memory tools included) before being registered.
    let all_mcp: Vec<_> = config
        .mcp_servers
        .iter()
        .chain(plugin_mcp_configs.iter())
        .cloned()
        .collect();
    let (_mcp_services, mcp_statuses) =
        crate::mcp::connect_servers(&all_mcp, tool_handle.clone(), gate_ctx.clone()).await;

    let project_ctx = read_project_instructions();
    let preamble = build_preamble(&project_ctx);

    let ts = |enabled: bool| {
        if enabled {
            Some(tool_handle.clone())
        } else {
            None
        }
    };

    let mut tools_enabled = true;

    // `startup` was already resolved and validated by `preflight_startup`
    // before the TUI opened, so there is no second-chance fallback here: a
    // failure now means something changed underneath us, and silently
    // substituting a different provider is exactly the behaviour being removed.
    let (mut agent, mut current_model) = match build_startup_agent(
        &startup,
        &config,
        &preamble,
        &project_ctx,
        ts(tools_enabled),
    ) {
        Ok(pair) => pair,
        Err(e) => {
            ai_tx
                .send(AiEvent::Error(format!(
                    "startup: could not initialize model {:?}: {e}",
                    startup.display
                )))
                .ok();
            return;
        }
    };
    // What to rebuild from when only the tool server changes. Derived from the
    // choice itself, never re-parsed from `current_model`: that display string
    // is `provider/model` after `/provider`, which matches no alias and used to
    // send `/tools on|off` straight back to Ollama.
    let mut selection = match &startup.alias {
        Some(alias) => ModelSelection::Alias(alias.clone()),
        None => ModelSelection::Direct(startup.clone()),
    };

    hook_runner.fire(
        HookEvent::SessionStart,
        HashMap::from([("MAGAI_MODEL".to_string(), current_model.clone())]),
    );

    let session_id: Option<String> = if config.quality.enabled {
        memory_db
            .as_ref()
            .map(|db| crate::memory::quality::start_session(db, &current_model))
    } else {
        None
    };
    let mut turn_seq: usize = 0;

    let mut history: Vec<Message> = Vec::new();
    let mut tool_timings: HashMap<String, Instant> = HashMap::new();
    // Set when a turn is paused on `AiEvent::MaxTurnsReached`; holds the
    // still-unsent prompt so `MaxTurnsResponse` can resume or discard it.
    let mut pending_resume: Option<Message> = None;

    // Per-turn snapshots live in a shadow repository outside the project, so a
    // dirty working tree is simply the baseline — nothing is asked of the user
    // and their own repository is never written to.
    let checkpoints: Option<Arc<CheckpointStore>> = if config.checkpoints.enabled {
        let session = session_id.clone().unwrap_or_else(|| "anon".to_string());
        match CheckpointStore::open(&config.checkpoints, &session) {
            Ok(store) => Some(Arc::new(store)),
            Err(e) => {
                ai_tx
                    .send(AiEvent::Error(format!("checkpoints unavailable: {e}")))
                    .ok();
                None
            }
        }
    } else {
        None
    };

    if let Some(store) = &checkpoints {
        let s = Arc::clone(store);
        // `add -A` walks the whole tree, so keep it off the reactor. Awaited so
        // the baseline is ordered before the first turn.
        let baseline = tokio::task::spawn_blocking(move || {
            let outcome = s.snapshot(&SnapshotMeta::baseline());
            let _ = s.prune(); // best-effort; never blocks a session
            outcome
        })
        .await;
        match baseline {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                ai_tx
                    .send(AiEvent::Error(format!("checkpoint baseline failed: {e}")))
                    .ok();
            }
            Err(e) => {
                ai_tx
                    .send(AiEvent::Error(format!("checkpoint task failed: {e}")))
                    .ok();
            }
        }
    }

    // The sha `/undo` last reverted, so a second consecutive `/undo` steps one
    // turn further back. Cleared whenever a new turn is snapshotted.
    let mut undo_cursor: Option<String> = None;
    // The prompt that started the current turn, used to label its checkpoint.
    let mut last_prompt_label = String::new();

    while let Some(cmd) = user_rx.recv().await {
        let message = match cmd {
            AgentCommand::SetModel(alias) => {
                match resolve_agent(&alias, &config, &preamble, &project_ctx, ts(tools_enabled)) {
                    Ok((new_agent, display)) => {
                        agent = new_agent;
                        current_model = display;
                        selection = ModelSelection::Alias(alias);
                    }
                    Err(e) => {
                        ai_tx.send(AiEvent::Error(e)).ok();
                    }
                }
                continue;
            }
            AgentCommand::SetTools(enabled) => {
                tools_enabled = enabled;
                let rebuilt = match &selection {
                    ModelSelection::Alias(alias) => {
                        resolve_agent(alias, &config, &preamble, &project_ctx, ts(tools_enabled))
                            .map(|(a, _)| a)
                    }
                    ModelSelection::Direct(startup) => build_startup_agent(
                        startup,
                        &config,
                        &preamble,
                        &project_ctx,
                        ts(tools_enabled),
                    )
                    .map(|(a, _)| a),
                };
                match rebuilt {
                    Ok(new_agent) => agent = new_agent,
                    Err(e) => {
                        ai_tx.send(AiEvent::Error(e)).ok();
                    }
                }
                continue;
            }
            AgentCommand::Clear => {
                history.clear();
                ai_tx.send(AiEvent::HistoryCleared).ok();
                continue;
            }
            AgentCommand::CheckpointList => {
                let msg = match &checkpoints {
                    Some(store) => {
                        let s = Arc::clone(store);
                        let max = config.checkpoints.max_list;
                        match tokio::task::spawn_blocking(move || s.list(max)).await {
                            Ok(Ok(cps)) => crate::checkpoint::render_list(&cps, epoch_secs()),
                            Ok(Err(e)) => format!("checkpoints: {e}"),
                            Err(e) => format!("checkpoints: {e}"),
                        }
                    }
                    None => "checkpoints are disabled".to_string(),
                };
                ai_tx.send(AiEvent::CheckpointList(msg)).ok();
                continue;
            }
            AgentCommand::CheckpointDiff(id) => {
                match &checkpoints {
                    Some(store) => {
                        let s = Arc::clone(store);
                        match tokio::task::spawn_blocking(move || s.show(id)).await {
                            Ok(Ok(text)) => {
                                ai_tx.send(AiEvent::CheckpointDiff(text)).ok();
                            }
                            Ok(Err(e)) => {
                                ai_tx.send(AiEvent::Error(format!("diff: {e}"))).ok();
                            }
                            Err(e) => {
                                ai_tx.send(AiEvent::Error(format!("diff: {e}"))).ok();
                            }
                        }
                    }
                    None => {
                        ai_tx
                            .send(AiEvent::Error("checkpoints are disabled".into()))
                            .ok();
                    }
                }
                continue;
            }
            AgentCommand::CheckpointPreview(action) => {
                let Some(store) = &checkpoints else {
                    ai_tx
                        .send(AiEvent::Error("checkpoints are disabled".into()))
                        .ok();
                    continue;
                };
                let s = Arc::clone(store);
                let cursor = undo_cursor.clone();
                let planned =
                    tokio::task::spawn_blocking(move || s.plan(action, cursor.as_deref())).await;
                match planned {
                    Ok(Ok(plan)) => {
                        ai_tx
                            .send(AiEvent::CheckpointPreview {
                                action,
                                title: describe_action(action, &plan.target),
                                lines: crate::checkpoint::render_stat(&plan.stat, 10),
                                blocked: plan.blocked.clone(),
                            })
                            .ok();
                    }
                    Ok(Err(e)) => {
                        ai_tx.send(AiEvent::Error(e.to_string())).ok();
                    }
                    Err(e) => {
                        ai_tx.send(AiEvent::Error(format!("checkpoint: {e}"))).ok();
                    }
                }
                continue;
            }
            AgentCommand::CheckpointApply(action) => {
                let Some(store) = &checkpoints else {
                    ai_tx
                        .send(AiEvent::Error("checkpoints are disabled".into()))
                        .ok();
                    continue;
                };
                let s = Arc::clone(store);
                let cursor = undo_cursor.clone();
                // Re-planned rather than carried across the channel: the tree
                // may have moved since the preview, and `apply` snapshots first
                // so the action is itself reversible.
                let applied = tokio::task::spawn_blocking(move || {
                    let plan = s.plan(action, cursor.as_deref())?;
                    let target = plan.target.sha.clone();
                    s.apply(&plan).map(|(stat, safety)| (stat, target, safety))
                })
                .await;
                match applied {
                    Ok(Ok((stat, target, safety))) => {
                        match action {
                            crate::checkpoint::CheckpointAction::Undo => undo_cursor = Some(target),
                            _ => undo_cursor = None,
                        }
                        let verb = match action {
                            crate::checkpoint::CheckpointAction::Undo => "reverted",
                            crate::checkpoint::CheckpointAction::Redo => "re-applied",
                            crate::checkpoint::CheckpointAction::Restore(_) => "restored",
                        };
                        let summary = crate::checkpoint::render_stat(&stat, 10).join("\n");
                        let undo_hint = safety
                            .map(|id| format!("\n  (/restore {id} to put it back)"))
                            .unwrap_or_default();
                        ai_tx
                            .send(AiEvent::Notice(format!("{verb}:\n{summary}{undo_hint}")))
                            .ok();
                    }
                    Ok(Err(e)) => {
                        ai_tx.send(AiEvent::Error(e.to_string())).ok();
                    }
                    Err(e) => {
                        ai_tx.send(AiEvent::Error(format!("checkpoint: {e}"))).ok();
                    }
                }
                continue;
            }
            AgentCommand::MaxTurnsResponse(resp) => {
                let Some(prompt) = pending_resume.take() else {
                    continue;
                };
                match resp {
                    None => {
                        // No snapshot here: declining does no model work, and
                        // the turn that hit the cap was snapshotted already.
                        let turn_start_idx = history.len();
                        let turn_started_at = epoch_secs();
                        history.push(prompt);
                        finish_turn(
                            DriveResult::Done,
                            &[],
                            &history,
                            turn_start_idx,
                            turn_started_at,
                            &mut pending_resume,
                            &memory_db,
                            &config,
                            &current_model,
                            &ai_tx,
                            &hook_runner,
                            &tool_outcomes,
                            &session_id,
                            &mut turn_seq,
                        );
                    }
                    Some(n) => {
                        // No agent rebuild needed: `max_turns` is a per-call
                        // `.multi_turn()` cap on the request, not a property
                        // baked into the agent at build time.
                        let turn_start_idx = history.len();
                        let turn_started_at = epoch_secs();
                        let (result, tool_records) = drive_turn(
                            &agent,
                            prompt,
                            &mut history,
                            &ai_tx,
                            &mut user_rx,
                            &gate,
                            &mut tool_timings,
                            &current_model,
                            n,
                        )
                        .await;
                        let outcome = drive_outcome(&result);
                        finish_turn(
                            result,
                            &tool_records,
                            &history,
                            turn_start_idx,
                            turn_started_at,
                            &mut pending_resume,
                            &memory_db,
                            &config,
                            &current_model,
                            &ai_tx,
                            &hook_runner,
                            &tool_outcomes,
                            &session_id,
                            &mut turn_seq,
                        );
                        snapshot_turn(
                            &checkpoints,
                            &ai_tx,
                            outcome,
                            &last_prompt_label,
                            &current_model,
                            turn_seq,
                        )
                        .await;
                        undo_cursor = None;
                    }
                }
                continue;
            }
            AgentCommand::ListMcp => {
                ai_tx
                    .send(AiEvent::McpStatus(crate::mcp::summary(&mcp_statuses).await))
                    .ok();
                continue;
            }
            AgentCommand::ListModels(alias) => {
                let ai_tx2 = ai_tx.clone();
                let config2 = config.clone();
                tokio::spawn(async move {
                    match fetch_models(&alias, &config2).await {
                        Ok(models) => {
                            ai_tx2
                                .send(AiEvent::ModelList {
                                    provider: alias,
                                    models,
                                })
                                .ok();
                        }
                        Err(e) => {
                            ai_tx2
                                .send(AiEvent::Error(format!("provider list: {e}")))
                                .ok();
                        }
                    }
                });
                continue;
            }
            AgentCommand::UseProviderModel {
                provider_alias,
                model_id,
            } => {
                // Describe the pick structurally so `/tools on|off` can rebuild
                // it later without re-parsing the `provider/model` label.
                let picked = match config.providers.get(&provider_alias) {
                    Some(pc) => Ok(crate::config::StartupModel {
                        display: format!("{provider_alias}/{model_id}"),
                        alias: None,
                        provider_type: pc.provider_type,
                        model: model_id.clone(),
                        api_key_env: pc.api_key_env.clone(),
                        base_url: pc.base_url.clone(),
                        api: pc.api,
                    }),
                    None if provider_alias == "ollama" && config.ollama_is_available() => {
                        Ok(crate::config::StartupModel {
                            display: format!("{provider_alias}/{model_id}"),
                            alias: None,
                            provider_type: ProviderType::Ollama,
                            model: model_id.clone(),
                            api_key_env: None,
                            base_url: None,
                            api: Default::default(),
                        })
                    }
                    None => Err(format!(
                        "no provider configured for '{provider_alias}'. Configured: {}",
                        config.provider_names().join(", ")
                    )),
                };
                let result = picked.and_then(|startup| {
                    build_startup_agent(
                        &startup,
                        &config,
                        &preamble,
                        &project_ctx,
                        ts(tools_enabled),
                    )
                    .map(|(a, display)| (a, display, startup))
                });
                match result {
                    Ok((new_agent, display, startup)) => {
                        agent = new_agent;
                        current_model = display;
                        selection = ModelSelection::Direct(startup);
                    }
                    Err(e) => {
                        ai_tx.send(AiEvent::Error(e)).ok();
                    }
                }
                continue;
            }
            // These arrive out-of-stream; ignore (processed inside drive_stream when in-flight)
            AgentCommand::ApproveToolCall(_)
            | AgentCommand::DenyToolCall(_)
            | AgentCommand::Cancel => continue,
            AgentCommand::Message(msg) => msg,
        };

        // Label this turn's checkpoint with the user's prompt, captured before
        // memory context gets prepended to it.
        last_prompt_label = message.clone();

        // A new message while a max-turns prompt is outstanding implicitly
        // declines it: stash the unsent prompt into history rather than
        // silently dropping it, then handle the new message normally.
        if let Some(prompt) = pending_resume.take() {
            history.push(prompt);
        }

        // Pre-turn: prepend relevant memory context so the model has cross-session recall.
        let message = if let (Some(db), true) = (&memory_db, config.memory.inject_context) {
            let snippets = crate::memory::retrieval::query_relevant(
                db,
                &message,
                config.memory.max_context_snippets,
            );
            if !snippets.is_empty() {
                let ctx = snippets
                    .iter()
                    .map(|s| format!("- {}: {}", s.kind, s.name))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("Context from memory:\n{ctx}\n\n---\n\n{message}")
            } else {
                message
            }
        } else {
            message
        };

        let dropped = trim_history(&mut history, config.max_context_tokens);
        if dropped > 0 {
            ai_tx
                .send(AiEvent::ContextTruncated {
                    turns_dropped: dropped,
                })
                .ok();
        }

        let turn_start_idx = history.len();
        let turn_started_at = epoch_secs();
        let (result, tool_records) = drive_turn(
            &agent,
            message,
            &mut history,
            &ai_tx,
            &mut user_rx,
            &gate,
            &mut tool_timings,
            &current_model,
            config.max_turns,
        )
        .await;

        let outcome = drive_outcome(&result);
        finish_turn(
            result,
            &tool_records,
            &history,
            turn_start_idx,
            turn_started_at,
            &mut pending_resume,
            &memory_db,
            &config,
            &current_model,
            &ai_tx,
            &hook_runner,
            &tool_outcomes,
            &session_id,
            &mut turn_seq,
        );
        snapshot_turn(
            &checkpoints,
            &ai_tx,
            outcome,
            &last_prompt_label,
            &current_model,
            turn_seq,
        )
        .await;
        // A fresh turn invalidates the undo cursor: /undo targets it next.
        undo_cursor = None;
    }

    if let (Some(db), Some(id)) = (&memory_db, &session_id) {
        crate::memory::quality::end_session(db, id);
    }

    hook_runner.fire(
        HookEvent::SessionStop,
        HashMap::from([("MAGAI_MODEL".to_string(), current_model.clone())]),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StartupModel;

    fn ollama_choice(model: &str) -> StartupModel {
        StartupModel {
            display: model.to_string(),
            alias: None,
            provider_type: ProviderType::Ollama,
            model: model.to_string(),
            api_key_env: None,
            base_url: None,
            api: Default::default(),
        }
    }

    fn tags(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reconcile_keeps_a_model_the_server_has() {
        let m = ollama_choice("granite4:latest");
        let warning =
            reconcile_ollama_model(&m, &tags(&["llama3.2:latest", "granite4:latest"]), "u")
                .unwrap();
        assert!(warning.is_none());
        assert_eq!(m.model, "granite4:latest");
    }

    #[test]
    fn reconcile_never_substitutes_a_different_model() {
        // magai reports and warns, but the model the user named is the model
        // that runs — silently swapping in another is the behaviour this
        // whole path exists to remove.
        let m = ollama_choice("granite4:latest");
        let warning = reconcile_ollama_model(&m, &tags(&["llama3.2:latest"]), "u")
            .unwrap()
            .expect("a tag the server lacks should warn");
        assert!(warning.contains("granite4:latest"), "{warning}");
        assert_eq!(m.model, "granite4:latest");
    }

    #[test]
    fn reconcile_warns_but_keeps_an_explicit_model() {
        let m = ollama_choice("qwen3:8b");
        let warning = reconcile_ollama_model(&m, &tags(&["llama3.2:latest"]), "u")
            .unwrap()
            .expect("an explicit miss should warn");
        assert!(warning.contains("qwen3:8b"), "{warning}");
        assert_eq!(m.model, "qwen3:8b", "an explicit choice is never swapped");
    }

    #[test]
    fn reconcile_rejects_a_server_with_no_models() {
        let m = ollama_choice("granite4:latest");
        let err = reconcile_ollama_model(&m, &[], "http://h:11434").unwrap_err();
        assert!(err.contains("ollama pull"), "{err}");
    }
}
