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
use crate::config::{Config, ProviderType};
use crate::hooks::{HookEvent, HookRunner};
use crate::memory::{default_db_path, MemoryDb, ToolCallRecord};
use crate::ui::AiEvent;

use providers::{
    api_key_from_env, build_anthropic, build_ollama, build_openai, fetch_models, resolve_agent,
    DynAgent,
};
use stream::{drive_stream, DriveResult};

pub const DEFAULT_MODEL: &str = "granite4:latest";

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
    Undo,
    Squash(String),
    DirtyWorkspaceResponse(bool),
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

fn git_is_dirty() -> bool {
    std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .map(|out| !out.stdout.is_empty())
        .unwrap_or(false)
}

fn git_commit_checkpoint(ai_tx: &mpsc::UnboundedSender<AiEvent>) {
    let _ = std::process::Command::new("git")
        .args(["add", "-A"])
        .output();
    let has_staged = std::process::Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .status()
        .map(|s| !s.success())
        .unwrap_or(false);
    if !has_staged {
        return;
    }
    match std::process::Command::new("git")
        .args(["commit", "-m", "magai-checkpoint"])
        .output()
    {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            ai_tx
                .send(AiEvent::Error(format!(
                    "checkpoint failed (undo unavailable this turn): {}",
                    String::from_utf8_lossy(&out.stderr)
                )))
                .ok();
        }
        Err(e) => {
            ai_tx
                .send(AiEvent::Error(format!("checkpoint error: {e}")))
                .ok();
        }
    }
}

fn git_undo(ai_tx: &mpsc::UnboundedSender<AiEvent>) {
    match std::process::Command::new("git")
        .args(["reset", "--hard", "HEAD~1"])
        .output()
    {
        Ok(out) if out.status.success() => {
            ai_tx
                .send(AiEvent::Error("undo: restored previous state".into()))
                .ok();
        }
        Ok(out) => {
            ai_tx
                .send(AiEvent::Error(format!(
                    "undo failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                )))
                .ok();
        }
        Err(e) => {
            ai_tx.send(AiEvent::Error(format!("undo error: {e}"))).ok();
        }
    }
}

fn git_squash(start_sha: &str, message: &str, ai_tx: &mpsc::UnboundedSender<AiEvent>) {
    match std::process::Command::new("git")
        .args(["reset", "--soft", start_sha])
        .output()
    {
        Ok(out) if !out.status.success() => {
            ai_tx
                .send(AiEvent::Error(format!(
                    "squash reset failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                )))
                .ok();
            return;
        }
        Err(e) => {
            ai_tx
                .send(AiEvent::Error(format!("squash reset error: {e}")))
                .ok();
            return;
        }
        _ => {}
    }
    if message.is_empty() {
        ai_tx
            .send(AiEvent::Error(
                "squash: checkpoint commits collapsed — staged changes ready to commit".into(),
            ))
            .ok();
        return;
    }
    match std::process::Command::new("git")
        .args(["commit", "-m", message])
        .output()
    {
        Ok(out) if out.status.success() => {
            ai_tx
                .send(AiEvent::Error(format!(
                    "squash: committed as \"{message}\""
                )))
                .ok();
        }
        Ok(out) => {
            ai_tx
                .send(AiEvent::Error(format!(
                    "squash commit failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                )))
                .ok();
        }
        Err(e) => {
            ai_tx
                .send(AiEvent::Error(format!("squash commit error: {e}")))
                .ok();
        }
    }
}

fn is_git_repo() -> bool {
    std::path::Path::new(".git").exists()
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

/// Post-turn bookkeeping shared by every path that finishes driving a turn:
/// memory extraction, and — depending on outcome — either the checkpoint
/// commit + `AgentResponse` hook (`Done`) or stashing the resume prompt for
/// the next `MaxTurnsResponse` (`MaxTurnsReached`).
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
    checkpoint_enabled: bool,
    ai_tx: &mpsc::UnboundedSender<AiEvent>,
    hook_runner: &HookRunner,
    tool_outcomes: &ToolOutcomeCounter,
    session_id: &Option<String>,
    turn_seq: &mut usize,
) {
    if let Some(db) = memory_db {
        crate::memory::extract::process_turn(db, current_model, tool_records);

        // LLM-based fact extraction — fire-and-forget, opt-in via config.
        if let Some(model) = config.memory.extract_facts_model.clone() {
            let text = extract_final_assistant_text(history);
            if !text.is_empty() {
                let db = Arc::clone(db);
                let alias = current_model.to_string();
                tokio::spawn(crate::memory::extract::extract_facts_async(
                    db, text, model, alias,
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
                *turn_seq += 1;

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
                        tokio::spawn(crate::memory::quality::judge_turn_async(
                            db,
                            turn_id,
                            judge_model,
                            user_text,
                            assistant_text,
                        ));
                    }
                }
            }
        }
    }

    match result {
        DriveResult::Done => {
            if checkpoint_enabled {
                git_commit_checkpoint(ai_tx);
            }
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
                    .add_tool(
                        gate_ctx.wrap(
                            crate::tools::MemorySave::new(
                                db.clone(),
                                config
                                    .default_model
                                    .as_deref()
                                    .unwrap_or(DEFAULT_MODEL)
                                    .to_string(),
                            ),
                            false,
                        ),
                    )
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
    let startup = config.default_model.as_deref().unwrap_or(DEFAULT_MODEL);

    let (mut agent, mut current_model) =
        match resolve_agent(startup, &config, &preamble, &project_ctx, ts(tools_enabled)) {
            Ok(pair) => pair,
            Err(_) => match build_ollama(DEFAULT_MODEL, &preamble, Some(tool_handle.clone())) {
                Ok(agent) => (agent, DEFAULT_MODEL.to_string()),
                Err(e) => {
                    ai_tx
                        .send(AiEvent::Error(format!(
                            "startup: could not initialize any model agent: {e}"
                        )))
                        .ok();
                    return;
                }
            },
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

    let in_git = is_git_repo();
    let checkpointing_configured = in_git && config.git_checkpointing;

    let session_start_sha: Option<String> = if checkpointing_configured {
        std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .output()
            .ok()
            .and_then(|o| {
                o.status
                    .success()
                    .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
            })
    } else {
        None
    };

    let checkpoint_enabled = if checkpointing_configured && git_is_dirty() {
        ai_tx.send(AiEvent::DirtyWorkspacePrompt).ok();
        let mut enabled = false;
        loop {
            match user_rx.recv().await {
                Some(AgentCommand::DirtyWorkspaceResponse(stash)) => {
                    if stash {
                        let _ = std::process::Command::new("git")
                            .args([
                                "stash",
                                "push",
                                "--include-untracked",
                                "-m",
                                "magai-user-stash",
                            ])
                            .output();
                        enabled = true;
                    }
                    break;
                }
                None => return,
                _ => {}
            }
        }
        enabled
    } else {
        checkpointing_configured
    };

    while let Some(cmd) = user_rx.recv().await {
        let message = match cmd {
            AgentCommand::SetModel(alias) => {
                match resolve_agent(&alias, &config, &preamble, &project_ctx, ts(tools_enabled)) {
                    Ok((new_agent, display)) => {
                        agent = new_agent;
                        current_model = display;
                    }
                    Err(e) => {
                        ai_tx.send(AiEvent::Error(e)).ok();
                    }
                }
                continue;
            }
            AgentCommand::SetTools(enabled) => {
                tools_enabled = enabled;
                match resolve_agent(
                    &current_model,
                    &config,
                    &preamble,
                    &project_ctx,
                    ts(tools_enabled),
                ) {
                    Ok((new_agent, _)) => agent = new_agent,
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
            AgentCommand::Undo => {
                if checkpoint_enabled {
                    git_undo(&ai_tx);
                } else if !in_git {
                    ai_tx
                        .send(AiEvent::Error(
                            "undo is not available (not a git repository)".into(),
                        ))
                        .ok();
                } else {
                    ai_tx
                        .send(AiEvent::Error(
                            "undo is not available (session started with uncommitted changes)"
                                .into(),
                        ))
                        .ok();
                }
                continue;
            }
            AgentCommand::Squash(message) => {
                if let Some(ref sha) = session_start_sha {
                    if checkpoint_enabled {
                        git_squash(sha, &message, &ai_tx);
                    } else {
                        ai_tx
                            .send(AiEvent::Error(
                                "squash is not available (checkpointing disabled)".into(),
                            ))
                            .ok();
                    }
                } else {
                    ai_tx
                        .send(AiEvent::Error(
                            "squash is not available (not a git repository)".into(),
                        ))
                        .ok();
                }
                continue;
            }
            AgentCommand::DirtyWorkspaceResponse(_) => continue,
            AgentCommand::MaxTurnsResponse(resp) => {
                let Some(prompt) = pending_resume.take() else {
                    continue;
                };
                match resp {
                    None => {
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
                            checkpoint_enabled,
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
                            checkpoint_enabled,
                            &ai_tx,
                            &hook_runner,
                            &tool_outcomes,
                            &session_id,
                            &mut turn_seq,
                        );
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
                let result = if let Some(pc) = config.providers.get(&provider_alias) {
                    match pc.provider_type {
                        ProviderType::OpenAI | ProviderType::Groq => {
                            api_key_from_env(pc.api_key_env.as_deref()).and_then(|k| {
                                build_openai(
                                    &model_id,
                                    &k,
                                    pc.base_url.as_deref(),
                                    &preamble,
                                    ts(tools_enabled),
                                )
                            })
                        }
                        ProviderType::Anthropic => api_key_from_env(pc.api_key_env.as_deref())
                            .and_then(|k| {
                                build_anthropic(&model_id, &k, &preamble, ts(tools_enabled))
                            }),
                        ProviderType::Ollama | ProviderType::Gemini => {
                            build_ollama(&model_id, &preamble, ts(tools_enabled))
                        }
                    }
                } else {
                    build_ollama(&model_id, &preamble, ts(tools_enabled))
                };
                match result {
                    Ok(new_agent) => {
                        agent = new_agent;
                        current_model = format!("{provider_alias}/{model_id}");
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
            checkpoint_enabled,
            &ai_tx,
            &hook_runner,
            &tool_outcomes,
            &session_id,
            &mut turn_seq,
        );
    }

    if let (Some(db), Some(id)) = (&memory_db, &session_id) {
        crate::memory::quality::end_session(db, id);
    }

    hook_runner.fire(
        HookEvent::SessionStop,
        HashMap::from([("MAGAI_MODEL".to_string(), current_model.clone())]),
    );
}
