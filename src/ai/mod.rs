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

use crate::approval::{ApprovalGate, GatedTool, PermissionMode};
use crate::config::{Config, ProviderType};
use crate::hooks::{HookEvent, HookRunner};
use crate::ui::AiEvent;

use providers::{
    api_key_from_env, build_anthropic, build_ollama, build_openai, fetch_models, resolve_agent,
};
use stream::{drive_stream, DriveResult};

pub const DEFAULT_MODEL: &str = "granite4:latest";
const PREAMBLE: &str =
    "You are a helpful coding agent. \
     For file operations use read_file, read_file_range, write_file, edit_file, or list_directory. \
     Reserve shell_command for build/test/install/git commands and other tasks no dedicated tool covers.";

pub enum AgentCommand {
    Message(String),
    SetModel(String),
    SetTools(bool),
    ApproveToolCall(String),
    DenyToolCall(String),
    Cancel,
    Clear,
    Undo,
    ListModels(String),
    UseProviderModel {
        provider_alias: String,
        model_id: String,
    },
}

/// Combines the base system preamble with project-specific instructions
/// from `AGENTS.md`/`CLAUDE.md`, if present.
fn build_preamble(project_ctx: &str) -> String {
    if project_ctx.trim().is_empty() {
        PREAMBLE.to_string()
    } else {
        format!("{PREAMBLE}\n\n# Project instructions\n\n{project_ctx}")
    }
}

// ── tool server ───────────────────────────────────────────────────────────────

async fn build_tool_server(
    mode: PermissionMode,
    gate: ApprovalGate,
    ai_tx: mpsc::UnboundedSender<AiEvent>,
    hook_runner: Arc<HookRunner>,
) -> ToolServerHandle {
    let handle = ToolServer::new().run();

    macro_rules! add {
        ($tool:expr, $dangerous:expr) => {
            handle
                .add_tool(GatedTool::new(
                    $tool,
                    $dangerous,
                    mode,
                    gate.clone(),
                    ai_tx.clone(),
                    hook_runner.clone(),
                ))
                .await
                .ok();
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

fn git_checkpoint() {
    let _ = std::process::Command::new("git")
        .args([
            "stash",
            "push",
            "--include-untracked",
            "-m",
            "magai-checkpoint",
        ])
        .output();
}

fn git_undo(ai_tx: &mpsc::UnboundedSender<AiEvent>) {
    match std::process::Command::new("git")
        .args(["stash", "pop"])
        .output()
    {
        Ok(out) if out.status.success() => {
            ai_tx
                .send(AiEvent::Error("undo: restored previous state".into()))
                .ok();
        }
        Ok(out) => {
            let msg = String::from_utf8_lossy(&out.stderr).to_string();
            ai_tx
                .send(AiEvent::Error(format!("undo failed: {msg}")))
                .ok();
        }
        Err(e) => {
            ai_tx.send(AiEvent::Error(format!("undo error: {e}"))).ok();
        }
    }
}

fn is_git_repo() -> bool {
    std::path::Path::new(".git").exists()
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

    let mode = config.permission_mode;
    let gate: ApprovalGate = Arc::new(Mutex::new(HashMap::new()));
    let tool_handle =
        build_tool_server(mode, gate.clone(), ai_tx.clone(), hook_runner.clone()).await;

    let all_mcp: Vec<_> = config
        .mcp_servers
        .iter()
        .chain(plugin_mcp_configs.iter())
        .cloned()
        .collect();
    let _mcp_services = crate::mcp::connect_servers(&all_mcp, tool_handle.clone()).await;

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
        match resolve_agent(startup, &config, &preamble, ts(tools_enabled)) {
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

    let mut history: Vec<Message> = Vec::new();
    let mut tool_timings: HashMap<String, Instant> = HashMap::new();
    let in_git = is_git_repo();

    while let Some(cmd) = user_rx.recv().await {
        let message = match cmd {
            AgentCommand::SetModel(alias) => {
                match resolve_agent(&alias, &config, &preamble, ts(tools_enabled)) {
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
                match resolve_agent(&current_model, &config, &preamble, ts(tools_enabled)) {
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
                git_undo(&ai_tx);
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

        if in_git {
            git_checkpoint();
        }

        let dropped = trim_history(&mut history, config.max_context_tokens);
        if dropped > 0 {
            ai_tx
                .send(AiEvent::ContextTruncated {
                    turns_dropped: dropped,
                })
                .ok();
        }

        ai_tx
            .send(AiEvent::ResponseStart(current_model.clone()))
            .ok();

        let taken = std::mem::take(&mut history);
        let stream = agent.stream_chat(message, taken).await;
        let result = drive_stream(
            stream,
            &ai_tx,
            &mut user_rx,
            &mut history,
            &gate,
            &mut tool_timings,
        )
        .await;

        if matches!(result, DriveResult::Done) {
            hook_runner.fire(
                HookEvent::AgentResponse,
                HashMap::from([("MAGAI_MODEL".to_string(), current_model.clone())]),
            );
        }
    }

    hook_runner.fire(
        HookEvent::SessionStop,
        HashMap::from([("MAGAI_MODEL".to_string(), current_model.clone())]),
    );
}
