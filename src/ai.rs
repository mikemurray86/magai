use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures::{future::BoxFuture, Stream, StreamExt};
use rig::agent::MultiTurnStreamItem;
use rig::client::{CompletionClient, ModelListingClient, Nothing};
use rig::message::{Message, ToolResultContent};
use rig::providers::{anthropic, ollama, openai};
use rig::streaming::{StreamedAssistantContent, StreamedUserContent, StreamingChat};
use rig::tool::server::{ToolServer, ToolServerHandle};
use tokio::sync::mpsc;

use crate::approval::{ApprovalGate, GatedTool, PermissionMode};
use crate::config::{Config, ProviderConfig, ProviderType};
use crate::hooks::{HookEvent, HookRunner};
use crate::ui::AiEvent;

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

// ── unified stream item ───────────────────────────────────────────────────────

enum OurItem {
    Text(String),
    History(Vec<Message>),
    Error(String),
    ToolCallStart {
        call_id: String,
        name: String,
        args_json: String,
    },
    ToolCallResult {
        call_id: String,
        result: String,
    },
}

type OurStream = Pin<Box<dyn Stream<Item = OurItem> + Send>>;

fn map_item<R, E: std::fmt::Display>(item: Result<MultiTurnStreamItem<R>, E>) -> Option<OurItem> {
    match item {
        Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(t))) => {
            Some(OurItem::Text(t.text))
        }
        Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
            tool_call,
            internal_call_id,
        })) => Some(OurItem::ToolCallStart {
            call_id: internal_call_id,
            name: tool_call.function.name,
            args_json: tool_call.function.arguments.to_string(),
        }),
        Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
            tool_result,
            internal_call_id,
        })) => {
            let result = tool_result
                .content
                .iter()
                .filter_map(|c| match c {
                    ToolResultContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            Some(OurItem::ToolCallResult {
                call_id: internal_call_id,
                result,
            })
        }
        Ok(MultiTurnStreamItem::FinalResponse(fin)) => Some(OurItem::History(
            fin.history().map(|h| h.to_vec()).unwrap_or_default(),
        )),
        Err(e) => Some(OurItem::Error(e.to_string())),
        _ => None,
    }
}

// ── type-erased agent ─────────────────────────────────────────────────────────

struct DynAgent(Box<dyn Fn(String, Vec<Message>) -> BoxFuture<'static, OurStream> + Send + Sync>);

impl DynAgent {
    async fn stream_chat(&self, msg: String, hist: Vec<Message>) -> OurStream {
        (self.0)(msg, hist).await
    }
}

// ── provider builders ─────────────────────────────────────────────────────────

fn build_ollama(model: &str, preamble: &str, tool_server: Option<ToolServerHandle>) -> DynAgent {
    let client = ollama::Client::new(Nothing).unwrap();
    let b = client.agent(model).preamble(preamble);
    let agent = match tool_server {
        Some(handle) => b.tool_server_handle(handle).build(),
        None => b.build(),
    };
    let agent = Arc::new(agent);
    DynAgent(Box::new(move |msg, hist| {
        let agent = Arc::clone(&agent);
        Box::pin(async move {
            let raw = agent.stream_chat(msg, hist).await;
            Box::pin(raw.filter_map(|item| async move { map_item(item) })) as OurStream
        })
    }))
}

fn build_openai(
    model: &str,
    api_key: &str,
    base_url: Option<&str>,
    preamble: &str,
    tool_server: Option<ToolServerHandle>,
) -> Result<DynAgent, String> {
    let mut builder = openai::CompletionsClient::builder().api_key(api_key);
    if let Some(url) = base_url {
        builder = builder.base_url(url);
    }
    let client = builder.build().map_err(|e| e.to_string())?;
    let b = client.agent(model).preamble(preamble);
    let agent = match tool_server {
        Some(handle) => b.tool_server_handle(handle).build(),
        None => b.build(),
    };
    let agent = Arc::new(agent);
    Ok(DynAgent(Box::new(move |msg, hist| {
        let agent = Arc::clone(&agent);
        Box::pin(async move {
            let raw = agent.stream_chat(msg, hist).await;
            Box::pin(raw.filter_map(|item| async move { map_item(item) })) as OurStream
        })
    })))
}

fn build_anthropic(
    model: &str,
    api_key: &str,
    preamble: &str,
    tool_server: Option<ToolServerHandle>,
) -> Result<DynAgent, String> {
    let client = anthropic::Client::new(api_key).map_err(|e| e.to_string())?;
    let b = client.agent(model).preamble(preamble);
    let agent = match tool_server {
        Some(handle) => b.tool_server_handle(handle).build(),
        None => b.build(),
    };
    let agent = Arc::new(agent);
    Ok(DynAgent(Box::new(move |msg, hist| {
        let agent = Arc::clone(&agent);
        Box::pin(async move {
            let raw = agent.stream_chat(msg, hist).await;
            Box::pin(raw.filter_map(|item| async move { map_item(item) })) as OurStream
        })
    })))
}

fn resolve_agent(
    alias: &str,
    config: &Config,
    preamble: &str,
    tool_server: Option<ToolServerHandle>,
) -> Result<(DynAgent, String), String> {
    match config.find_named_model(alias) {
        Some((nm, pc)) => {
            let agent = match pc.provider_type {
                ProviderType::Ollama => Ok(build_ollama(&nm.model, preamble, tool_server)),
                ProviderType::OpenAI | ProviderType::Groq => {
                    let key = api_key_from_env(pc.api_key_env.as_deref())?;
                    build_openai(
                        &nm.model,
                        &key,
                        pc.base_url.as_deref(),
                        preamble,
                        tool_server,
                    )
                }
                ProviderType::Anthropic => {
                    let key = api_key_from_env(pc.api_key_env.as_deref())?;
                    build_anthropic(&nm.model, &key, preamble, tool_server)
                }
                ProviderType::Gemini => Err(format!(
                    "provider {:?} is not yet supported",
                    pc.provider_type
                )),
            }?;
            Ok((agent, alias.to_string()))
        }
        None => Ok((
            build_ollama(alias, preamble, tool_server),
            alias.to_string(),
        )),
    }
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

async fn fetch_ollama_models() -> Result<Vec<String>, String> {
    let resp = reqwest::get("http://localhost:11434/api/tags")
        .await
        .map_err(|e| e.to_string())?;
    let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(json["models"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|m| m["name"].as_str().map(|s| s.to_string()))
        .collect())
}

async fn fetch_openai_models(pc: &ProviderConfig) -> Result<Vec<String>, String> {
    let key = api_key_from_env(pc.api_key_env.as_deref())?;
    // ModelListingClient is implemented on Client (Responses API), not CompletionsClient
    let mut builder = openai::Client::builder().api_key(&key);
    if let Some(url) = &pc.base_url {
        builder = builder.base_url(url);
    }
    let client = builder.build().map_err(|e| e.to_string())?;
    let list = client.list_models().await.map_err(|e| e.to_string())?;
    Ok(list.iter().map(|m| m.id.clone()).collect())
}

async fn fetch_anthropic_models(pc: &ProviderConfig) -> Result<Vec<String>, String> {
    let key = api_key_from_env(pc.api_key_env.as_deref())?;
    let client = anthropic::Client::new(&key).map_err(|e| e.to_string())?;
    let list = client.list_models().await.map_err(|e| e.to_string())?;
    Ok(list.iter().map(|m| m.id.clone()).collect())
}

async fn fetch_models(provider_name: &str, config: &Config) -> Result<Vec<String>, String> {
    let pc = config.providers.get(provider_name).ok_or_else(|| {
        if provider_name == "ollama" {
            // built-in ollama, no config entry needed
            return String::new(); // sentinel — handled below
        }
        format!("no provider configured for '{provider_name}'")
    });
    match pc {
        Ok(pc) => match pc.provider_type {
            ProviderType::Ollama => fetch_ollama_models().await,
            ProviderType::OpenAI | ProviderType::Groq => fetch_openai_models(pc).await,
            ProviderType::Anthropic => fetch_anthropic_models(pc).await,
            ProviderType::Gemini => Err("Gemini model listing not supported".to_string()),
        },
        Err(e) if e.is_empty() => fetch_ollama_models().await,
        Err(e) => Err(e),
    }
}

fn api_key_from_env(env_var: Option<&str>) -> Result<String, String> {
    let var = env_var.ok_or_else(|| "api_key_env not set in config".to_string())?;
    std::env::var(var).map_err(|_| format!("env var {var} is not set"))
}

// ── stream driver ─────────────────────────────────────────────────────────────

enum DriveResult {
    Done,
    Cancelled,
}

async fn drive_stream(
    mut stream: OurStream,
    ai_tx: &mpsc::UnboundedSender<AiEvent>,
    user_rx: &mut mpsc::UnboundedReceiver<AgentCommand>,
    history: &mut Vec<Message>,
    gate: &ApprovalGate,
    tool_timings: &mut HashMap<String, Instant>,
) -> DriveResult {
    loop {
        tokio::select! {
            item = stream.next() => {
                match item {
                    None => {
                        ai_tx.send(AiEvent::Done).ok();
                        return DriveResult::Done;
                    }
                    Some(OurItem::Text(text)) => {
                        if ai_tx.send(AiEvent::Token(text)).is_err() {
                            return DriveResult::Done;
                        }
                    }
                    Some(OurItem::ToolCallStart { call_id, name, args_json }) => {
                        tool_timings.insert(call_id.clone(), Instant::now());
                        ai_tx.send(AiEvent::ToolCallStart {
                            call_id,
                            name,
                            args_json,
                        }).ok();
                    }
                    Some(OurItem::ToolCallResult { call_id, result }) => {
                        let elapsed_ms = tool_timings.remove(&call_id)
                            .map(|t| t.elapsed().as_millis() as u64)
                            .unwrap_or(0);
                        ai_tx.send(AiEvent::ToolCallResult { call_id, result, elapsed_ms }).ok();
                    }
                    Some(OurItem::History(h)) => {
                        *history = h;
                        ai_tx.send(AiEvent::Done).ok();
                        return DriveResult::Done;
                    }
                    Some(OurItem::Error(e)) => {
                        ai_tx.send(AiEvent::Error(e)).ok();
                        return DriveResult::Done;
                    }
                }
            }
            cmd = user_rx.recv() => {
                match cmd {
                    Some(AgentCommand::Cancel) => {
                        return DriveResult::Cancelled;
                    }
                    Some(AgentCommand::ApproveToolCall(id)) => {
                        if let Some(tx) = gate.lock().unwrap().remove(&id) {
                            tx.send(true).ok();
                        }
                    }
                    Some(AgentCommand::DenyToolCall(id)) => {
                        if let Some(tx) = gate.lock().unwrap().remove(&id) {
                            tx.send(false).ok();
                        }
                    }
                    None => return DriveResult::Done,
                    // Queue non-approval commands by ignoring them during stream
                    // (model/tools changes take effect after the current turn)
                    Some(_) => {}
                }
            }
        }
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
        resolve_agent(startup, &config, &preamble, ts(tools_enabled)).unwrap_or_else(|_| {
            (
                build_ollama(DEFAULT_MODEL, &preamble, Some(tool_handle.clone())),
                DEFAULT_MODEL.to_string(),
            )
        });

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
                            Ok(build_ollama(&model_id, &preamble, ts(tools_enabled)))
                        }
                    }
                } else {
                    Ok(build_ollama(&model_id, &preamble, ts(tools_enabled)))
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
