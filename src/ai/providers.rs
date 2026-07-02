//! Provider-specific agent construction. Wraps each `rig` provider client into
//! a type-erased `DynAgent` (via the shared `dyn_agent_from!` macro), resolves
//! config aliases to concrete agents (`resolve_agent`), and discovers available
//! models per-provider for `/model` listing.

use std::sync::Arc;

use futures::future::BoxFuture;
use futures::StreamExt;
use rig::client::{CompletionClient, ModelListingClient, Nothing};
use rig::message::Message;
use rig::providers::{anthropic, ollama, openai};
use rig::streaming::StreamingChat;
use rig::tool::server::ToolServerHandle;

use crate::config::{Config, ProviderConfig, ProviderType};

use super::stream::{map_item, OurStream};

// ── type-erased agent ─────────────────────────────────────────────────────────

/// A boxed, provider-agnostic streaming-chat closure: erases the concrete
/// `rig::Agent<M>` type (and its model-specific completion/response types) so
/// the rest of the code can hold "the current agent" without knowing which
/// provider backs it. Built once per provider via `dyn_agent_from!` and
/// rebuilt whenever the user switches models.
pub(crate) struct DynAgent(
    Box<dyn Fn(Message, Vec<Message>, usize) -> BoxFuture<'static, OurStream> + Send + Sync>,
);

impl DynAgent {
    /// `max_turns` is a per-call cap (via rig's `.multi_turn()`), not an
    /// agent-build-time setting: `AgentBuilder::default_max_turns` does not
    /// apply to the `StreamingChat` shorthand this wraps — a fresh
    /// `StreamingPromptRequest` always starts its own cap at 0 regardless of
    /// how the agent was built, so every call must set it explicitly. This
    /// also means resuming after `MaxTurnsReached` needs no agent rebuild —
    /// just a fresh call with a different `max_turns`.
    pub(crate) async fn stream_chat(
        &self,
        msg: impl Into<Message>,
        hist: Vec<Message>,
        max_turns: usize,
    ) -> OurStream {
        (self.0)(msg.into(), hist, max_turns).await
    }
}

/// Wraps a built `rig` agent in the type-erased `DynAgent` closure that
/// normalizes its stream items via `map_item`. Shared by all provider builders
/// so the stream-adaptation logic lives in exactly one place.
///
/// A macro rather than a generic fn: `StreamingChat<M, R>` carries two
/// generic parameters with model-specific bounds, so a generic function would
/// need to restate them; each call site already has a concrete agent type.
macro_rules! dyn_agent_from {
    ($agent:expr) => {{
        let agent = Arc::new($agent);
        DynAgent(Box::new(move |msg, hist, max_turns| {
            let agent = Arc::clone(&agent);
            Box::pin(async move {
                let raw = agent.stream_chat(msg, hist).multi_turn(max_turns).await;
                Box::pin(raw.filter_map(|item| async move { map_item(item) })) as OurStream
            })
        }))
    }};
}

// ── provider builders ─────────────────────────────────────────────────────────

pub(crate) fn build_ollama(
    model: &str,
    preamble: &str,
    tool_server: Option<ToolServerHandle>,
) -> Result<DynAgent, String> {
    let client = ollama::Client::new(Nothing).map_err(|e| e.to_string())?;
    let b = client.agent(model).preamble(preamble);
    let agent = match tool_server {
        Some(handle) => b.tool_server_handle(handle).build(),
        None => b.build(),
    };
    Ok(dyn_agent_from!(agent))
}

pub(crate) fn build_openai(
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
    Ok(dyn_agent_from!(agent))
}

pub(crate) fn build_anthropic(
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
    Ok(dyn_agent_from!(agent))
}

pub(crate) fn resolve_agent(
    alias: &str,
    config: &Config,
    preamble: &str,
    tool_server: Option<ToolServerHandle>,
) -> Result<(DynAgent, String), String> {
    match config.find_named_model(alias) {
        Some((nm, pc)) => {
            let agent = match pc.provider_type {
                ProviderType::Ollama => build_ollama(&nm.model, preamble, tool_server),
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
        None => build_ollama(alias, preamble, tool_server).map(|a| (a, alias.to_string())),
    }
}

// ── model discovery ───────────────────────────────────────────────────────────

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

pub(crate) async fn fetch_models(
    provider_name: &str,
    config: &Config,
) -> Result<Vec<String>, String> {
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

pub(crate) fn api_key_from_env(env_var: Option<&str>) -> Result<String, String> {
    let var = env_var.ok_or_else(|| "api_key_env not set in config".to_string())?;
    std::env::var(var).map_err(|_| format!("env var {var} is not set"))
}
