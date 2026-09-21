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

use crate::config::{
    ollama_base_url_from, Config, ProviderConfig, ProviderType, StartupModel, OLLAMA_BASE_URL_ENV,
};

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

/// Builds an Ollama agent against `base_url`. rig's `Client::new` hardcodes
/// `http://localhost:11434` and only reads `$OLLAMA_API_BASE_URL` from
/// `ProviderClient::from_env()`, which magai never calls — so the endpoint is
/// resolved here and threaded in (see `config::ollama_base_url_from`).
pub(crate) fn build_ollama(
    model: &str,
    base_url: &str,
    preamble: &str,
    tool_server: Option<ToolServerHandle>,
) -> Result<DynAgent, String> {
    let client = ollama::Client::builder()
        .api_key(Nothing)
        .base_url(base_url)
        .build()
        .map_err(|e| e.to_string())?;
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

/// Resolves `alias` to a built agent. If the alias names a `[[named_models]]`
/// entry with `system_prompt`/`system_prompt_file` set, that text replaces
/// `default_preamble` (still combined with `project_ctx`, per
/// `build_preamble_from`); otherwise `default_preamble` is used as-is.
pub(crate) fn resolve_agent(
    alias: &str,
    config: &Config,
    default_preamble: &str,
    project_ctx: &str,
    tool_server: Option<ToolServerHandle>,
) -> Result<(DynAgent, String), String> {
    match config.find_named_model(alias) {
        Some((nm, pc)) => {
            let preamble = match nm.resolve_system_prompt()? {
                Some(base) => super::build_preamble_from(&base, project_ctx),
                None => default_preamble.to_string(),
            };
            let agent = match pc.provider_type {
                ProviderType::Ollama => build_ollama(
                    &nm.model,
                    &ollama_base_url(config, pc.base_url.as_deref()),
                    &preamble,
                    tool_server,
                ),
                ProviderType::OpenAI | ProviderType::Groq => {
                    let key = api_key_from_env(pc.api_key_env.as_deref())?;
                    build_openai(
                        &nm.model,
                        &key,
                        pc.base_url.as_deref(),
                        &preamble,
                        tool_server,
                    )
                }
                ProviderType::Anthropic => {
                    let key = api_key_from_env(pc.api_key_env.as_deref())?;
                    build_anthropic(&nm.model, &key, &preamble, tool_server)
                }
                ProviderType::Gemini => Err(gemini_unsupported()),
            }?;
            Ok((agent, alias.to_string()))
        }
        // An alias with no `[[named_models]]` entry used to become an Ollama
        // tag unconditionally, which silently turned a typo — or a hosted
        // model id — into a broken local agent. Only do it where Ollama is
        // genuinely the implicit provider.
        None if config.ollama_is_available() => build_ollama(
            alias,
            &ollama_base_url(config, None),
            default_preamble,
            tool_server,
        )
        .map(|a| (a, alias.to_string())),
        None => Err(unknown_alias_message(alias, config)),
    }
}

/// Explains an unresolvable model name in terms of what this config actually
/// offers, rather than failing later with a connection error to a server the
/// user never asked for.
pub(crate) fn unknown_alias_message(alias: &str, config: &Config) -> String {
    let aliases: Vec<&str> = config
        .named_models
        .iter()
        .map(|m| m.alias.as_str())
        .collect();
    let known = if aliases.is_empty() {
        "(none)".to_string()
    } else {
        aliases.join(", ")
    };
    format!(
        "unknown model {alias:?}. Configured aliases: {known}. Configured \
         providers: {}. Add a [[named_models]] entry for it, or browse a \
         provider's catalogue with /provider <name>.",
        config.provider_names().join(", ")
    )
}

/// The Ollama endpoint for a call, preferring a provider entry's own
/// `base_url` over the config-wide/env-wide resolution.
pub(crate) fn ollama_base_url(config: &Config, provider_base_url: Option<&str>) -> String {
    match provider_base_url.filter(|s| !s.trim().is_empty()) {
        Some(url) => ollama_base_url_from(
            Some(url),
            std::env::var(OLLAMA_BASE_URL_ENV).ok().as_deref(),
        ),
        None => config.ollama_base_url(),
    }
}

/// Builds an agent from an already-resolved [`StartupModel`]. A choice that
/// came from a `[[named_models]]` entry is routed back through
/// [`resolve_agent`] so that entry's `system_prompt` override still applies.
pub(crate) fn build_startup_agent(
    startup: &StartupModel,
    config: &Config,
    default_preamble: &str,
    project_ctx: &str,
    tool_server: Option<ToolServerHandle>,
) -> Result<(DynAgent, String), String> {
    if let Some(alias) = &startup.alias {
        return resolve_agent(alias, config, default_preamble, project_ctx, tool_server);
    }
    let agent = match startup.provider_type {
        ProviderType::Ollama => build_ollama(
            &startup.model,
            &ollama_base_url(config, startup.base_url.as_deref()),
            default_preamble,
            tool_server,
        ),
        ProviderType::OpenAI | ProviderType::Groq => {
            let key = api_key_from_env(startup.api_key_env.as_deref())?;
            build_openai(
                &startup.model,
                &key,
                startup.base_url.as_deref(),
                default_preamble,
                tool_server,
            )
        }
        ProviderType::Anthropic => {
            let key = api_key_from_env(startup.api_key_env.as_deref())?;
            build_anthropic(&startup.model, &key, default_preamble, tool_server)
        }
        ProviderType::Gemini => Err(gemini_unsupported()),
    }?;
    Ok((agent, startup.display.clone()))
}

/// magai ships no Gemini client. Said once, so every dispatch site agrees —
/// `UseProviderModel` used to route Gemini ids to Ollama instead.
pub(crate) fn gemini_unsupported() -> String {
    "provider type 'gemini' is not supported yet — magai has no Gemini client. \
     Use anthropic, openai, groq, or ollama."
        .to_string()
}

// ── model discovery ───────────────────────────────────────────────────────────

/// Lists the tags an Ollama server holds. Bounded by a connect timeout so a
/// black-holed host fails fast instead of leaving `/provider ollama` spinning
/// forever, and errors carry the URL so a wrong port is diagnosable. Doubles
/// as the startup reachability probe (`ai::preflight_startup`).
pub(crate) async fn fetch_ollama_models(base_url: &str) -> Result<Vec<String>, String> {
    let client = http_client()?;
    let resp = client
        .get(format!("{base_url}/api/tags"))
        .send()
        .await
        .map_err(|e| format!("ollama at {base_url}: {e}"))?;
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("ollama at {base_url}: {e}"))?;
    Ok(json["models"]
        .as_array()
        .map(|models| {
            models
                .iter()
                .filter_map(|m| m["name"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default())
}

/// `GET <base>/models` against any OpenAI-compatible endpoint.
///
/// Deliberately not rig's `list_models`: its response type requires
/// `owned_by`, which OpenAI itself sends but compatible endpoints (OpenRouter,
/// local proxies) do not — so listing failed outright for exactly the
/// providers `base_url` exists to support. Only `id` is required here, and
/// `created` is used when present to order newest-first.
async fn fetch_openai_models(pc: &ProviderConfig) -> Result<Vec<String>, String> {
    let key = api_key_from_env(pc.api_key_env.as_deref())?;
    let base = pc
        .base_url
        .as_deref()
        .unwrap_or("https://api.openai.com/v1")
        .trim_end_matches('/');
    let client = http_client()?;
    let resp = client
        .get(format!("{base}/models"))
        .bearer_auth(&key)
        .send()
        .await
        .map_err(|e| format!("{base}: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("{base}: HTTP {status}: {}", truncate(&body, 200)));
    }
    let json: serde_json::Value = resp.json().await.map_err(|e| format!("{base}: {e}"))?;
    let Some(data) = json["data"].as_array() else {
        return Err(format!("{base}: response had no \"data\" array"));
    };
    let mut models: Vec<(i64, String)> = data
        .iter()
        .filter_map(|m| {
            let id = m["id"].as_str()?.to_string();
            Some((m["created"].as_i64().unwrap_or(0), id))
        })
        .collect();
    models.sort_by_key(|(created, _)| std::cmp::Reverse(*created));
    Ok(models.into_iter().map(|(_, id)| id).collect())
}

/// A bounded HTTP client for catalogue lookups, so an unresponsive endpoint
/// fails fast instead of stalling startup or `/provider`.
fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(2))
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())
}

/// Caps text at `max` characters on a char boundary. Provider errors can embed
/// an entire response body — one was 724 KB — which would otherwise be dumped
/// into the user's terminal.
pub(crate) fn truncate(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}… (truncated)")
}

/// Model ids newest-first. Providers return their catalogue in arbitrary
/// order, and the newest entries are the ones a user is most likely to want
/// to see first when magai asks them to pick one.
fn newest_first(list: rig::model::ModelList) -> Vec<String> {
    let mut models = list.data;
    models.sort_by_key(|m| std::cmp::Reverse(m.created_at));
    models.into_iter().map(|m| m.id).collect()
}

async fn fetch_anthropic_models(pc: &ProviderConfig) -> Result<Vec<String>, String> {
    let key = api_key_from_env(pc.api_key_env.as_deref())?;
    let client = anthropic::Client::new(&key).map_err(|e| e.to_string())?;
    Ok(newest_first(
        client
            .list_models()
            .await
            .map_err(|e| truncate(&e.to_string(), 300))?,
    ))
}

/// Lists a provider's catalogue from its settings rather than its config key,
/// so a provider detected from the environment — which has no `[providers.*]`
/// entry — can be listed too.
pub(crate) async fn fetch_models_for(
    pc: &ProviderConfig,
    config: &Config,
) -> Result<Vec<String>, String> {
    match pc.provider_type {
        ProviderType::Ollama => {
            fetch_ollama_models(&ollama_base_url(config, pc.base_url.as_deref())).await
        }
        ProviderType::OpenAI | ProviderType::Groq => fetch_openai_models(pc).await,
        ProviderType::Anthropic => fetch_anthropic_models(pc).await,
        ProviderType::Gemini => Err("Gemini model listing not supported".to_string()),
    }
}

pub(crate) async fn fetch_models(
    provider_name: &str,
    config: &Config,
) -> Result<Vec<String>, String> {
    match config.providers.get(provider_name) {
        Some(pc) => fetch_models_for(pc, config).await,
        // Ollama needs no config entry, but only when it is the implicit
        // provider — otherwise it is just an unconfigured name like any other.
        None if provider_name == "ollama" && config.ollama_is_available() => {
            fetch_ollama_models(&ollama_base_url(config, None)).await
        }
        None => Err(format!(
            "no provider configured for '{provider_name}'. Configured: {}",
            config.provider_names().join(", ")
        )),
    }
}

pub(crate) fn api_key_from_env(env_var: Option<&str>) -> Result<String, String> {
    let var = env_var.ok_or_else(|| "api_key_env not set in config".to_string())?;
    std::env::var(var).map_err(|_| format!("env var {var} is not set"))
}
