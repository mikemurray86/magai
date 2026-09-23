use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProviderType {
    Ollama,
    OpenAI,
    Anthropic,
    Groq,
    Gemini,
}

impl ProviderType {
    /// The name this type is spelled with in `type = "..."`, reused for the
    /// `provider/model` labels shown in the TUI.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::OpenAI => "openai",
            Self::Anthropic => "anthropic",
            Self::Groq => "groq",
            Self::Gemini => "gemini",
        }
    }
}

/// Which OpenAI wire API an `openai`-type provider is spoken to over.
/// `chat` (`/chat/completions`) is what every OpenAI-compatible server
/// implements; `responses` (`/responses`) is required by some newer OpenAI
/// models, which reject Chat Completions outright — including when reached
/// through a proxy such as LiteLLM. Ignored for non-OpenAI provider types.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum OpenAIApi {
    #[default]
    Chat,
    Responses,
}

/// Where Ollama lives when nothing says otherwise.
pub const OLLAMA_DEFAULT_BASE_URL: &str = "http://localhost:11434";

/// The environment variable rig itself honours — but only in
/// `ProviderClient::from_env()`, which magai never calls.
pub const OLLAMA_BASE_URL_ENV: &str = "OLLAMA_API_BASE_URL";

/// Providers magai can detect from the environment when none are configured,
/// in precedence order. Deliberately only the env var that proves a provider
/// is usable, and no model id: magai does not guess which model to run, since
/// a baked-in id goes stale and may not exist on the account. The provider's
/// own catalogue is listed instead, and the user picks.
pub const AUTODETECT: &[(ProviderType, &str)] = &[
    (ProviderType::Anthropic, "ANTHROPIC_API_KEY"),
    (ProviderType::OpenAI, "OPENAI_API_KEY"),
    (ProviderType::Groq, "GROQ_API_KEY"),
];

/// Resolves the Ollama endpoint: `[providers.<ollama>].base_url` →
/// `$OLLAMA_API_BASE_URL` → [`OLLAMA_DEFAULT_BASE_URL`].
///
/// The trailing `/` is trimmed because rig's `build_uri` appends `"/" + path`
/// unconditionally — `"http://h:11434/"` would otherwise produce
/// `"http://h:11434//api/chat"`.
pub fn ollama_base_url_from(configured: Option<&str>, env: Option<&str>) -> String {
    let raw = configured
        .filter(|s| !s.trim().is_empty())
        .or(env.filter(|s| !s.trim().is_empty()))
        .unwrap_or(OLLAMA_DEFAULT_BASE_URL);
    raw.trim().trim_end_matches('/').to_string()
}

/// What magai decided to start with. `alias` is `Some` only when the choice
/// came from a `[[named_models]]` entry, so `resolve_agent` can still apply
/// that entry's `system_prompt`/`system_prompt_file` override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupModel {
    /// The label shown in the TUI.
    pub display: String,
    pub alias: Option<String>,
    pub provider_type: ProviderType,
    pub model: String,
    pub api_key_env: Option<String>,
    pub base_url: Option<String>,
    pub api: OpenAIApi,
}

/// The outcome of startup resolution. magai never picks a model on the user's
/// behalf: when nothing names one, it reports the provider's catalogue and
/// stops, rather than guessing an id that may be stale, absent from the
/// account, or unsuited to the work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupChoice {
    Model(StartupModel),
    NeedsModel(UnresolvedProvider),
}

/// A provider magai can reach, but with no model chosen for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedProvider {
    /// Name used in messages: the `[providers.*]` key, or the provider type
    /// when it was detected from the environment rather than configured.
    pub label: String,
    pub provider: ProviderConfig,
    /// How magai arrived here, quoted in the error so the cause is obvious.
    pub detected_via: String,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct ProviderConfig {
    #[serde(rename = "type")]
    pub provider_type: ProviderType,
    pub api_key_env: Option<String>,
    pub base_url: Option<String>,
    /// The default OpenAI wire API for this provider's models.
    #[serde(default)]
    pub api: OpenAIApi,
}

#[derive(Debug, Deserialize, Clone)]
pub struct NamedModel {
    pub alias: String,
    pub provider: String,
    pub model: String,
    /// Inline system prompt used instead of the default preamble
    /// (`src/ai/preamble.md`) when this model is active. Takes precedence
    /// over `system_prompt_file` if both are set.
    pub system_prompt: Option<String>,
    /// Path (relative to the working directory, or absolute) to a file
    /// whose contents replace the default preamble when this model is
    /// active. Ignored if `system_prompt` is also set.
    pub system_prompt_file: Option<String>,
    /// Overrides the provider's `api` for just this model, so one proxy can
    /// serve both Chat Completions and Responses-only models.
    pub api: Option<OpenAIApi>,
}

impl NamedModel {
    /// The OpenAI wire API to use for this model: its own `api`, else the
    /// provider's.
    pub fn api(&self, pc: &ProviderConfig) -> OpenAIApi {
        self.api.unwrap_or(pc.api)
    }

    /// Reads this model's custom system prompt, if configured. `system_prompt`
    /// wins over `system_prompt_file`; returns `Ok(None)` when neither is set.
    pub fn resolve_system_prompt(&self) -> Result<Option<String>, String> {
        if let Some(s) = &self.system_prompt {
            return Ok(Some(s.clone()));
        }
        if let Some(path) = &self.system_prompt_file {
            return std::fs::read_to_string(path).map(Some).map_err(|e| {
                format!(
                    "system_prompt_file {path:?} for model {:?}: {e}",
                    self.alias
                )
            });
        }
        Ok(None)
    }
}

/// An MCP server, reachable either as a local child process (`command`) or as a
/// remote streamable-HTTP endpoint (`url`). `url` wins if both are set.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct McpServerConfig {
    pub name: String,
    /// stdio transport: the executable to spawn.
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// streamable-HTTP transport: the endpoint URL.
    #[serde(default)]
    pub url: Option<String>,
    /// Extra HTTP headers sent with every request to a remote server.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Token sent as `Authorization: Bearer <token>` to a remote server.
    #[serde(default)]
    pub bearer_token: Option<String>,
    /// Exempt this server's tools from the approval gate. MCP tools are gated
    /// as dangerous by default, since what they do is opaque to magai.
    #[serde(default)]
    pub trusted: bool,
    /// How long to wait for the handshake before giving up on this server.
    /// Defaults to [`McpServerConfig::DEFAULT_TIMEOUT_SECS`] — generous, since
    /// a first `npx` run may download the server before it says anything.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// How to reach an [`McpServerConfig`], with `${VAR}` references already
/// expanded, so secrets can live in the environment rather than in the config.
#[derive(Debug, Clone, PartialEq)]
pub enum McpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
    Http {
        url: String,
        headers: HashMap<String, String>,
        bearer_token: Option<String>,
    },
}

/// Substitutes `${VAR}` with the value of environment variable `VAR` (empty
/// when unset). An unterminated `${` is left as written.
pub fn expand_env(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find("${") {
        let Some(end) = rest[start + 2..].find('}') else {
            break;
        };
        let var = &rest[start + 2..start + 2 + end];
        out.push_str(&rest[..start]);
        out.push_str(&std::env::var(var).unwrap_or_default());
        rest = &rest[start + 2 + end + 1..];
    }
    out.push_str(rest);
    out
}

impl McpServerConfig {
    pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

    /// Seconds to allow for connecting, before falling back to the default.
    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.timeout_secs.unwrap_or(Self::DEFAULT_TIMEOUT_SECS))
    }

    /// Which transport this entry describes, or a message naming what's missing.
    pub fn transport(&self) -> Result<McpTransport, String> {
        let expand_map = |m: &HashMap<String, String>| -> HashMap<String, String> {
            m.iter().map(|(k, v)| (k.clone(), expand_env(v))).collect()
        };
        if let Some(url) = &self.url {
            return Ok(McpTransport::Http {
                url: expand_env(url),
                headers: expand_map(&self.headers),
                bearer_token: self.bearer_token.as_deref().map(expand_env),
            });
        }
        let Some(command) = &self.command else {
            return Err(format!(
                "MCP server '{}': needs either `command` (stdio) or `url` (remote)",
                self.name
            ));
        };
        Ok(McpTransport::Stdio {
            command: expand_env(command),
            args: self.args.iter().map(|a| expand_env(a)).collect(),
            env: expand_map(&self.env),
        })
    }
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct Config {
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub named_models: Vec<NamedModel>,
    pub default_model: Option<String>,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
    #[serde(default)]
    pub permission_mode: crate::approval::PermissionMode,
    #[serde(default = "default_max_context_tokens")]
    pub max_context_tokens: usize,
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
    #[serde(default)]
    pub hooks: Vec<crate::hooks::HookConfig>,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub checkpoints: CheckpointsConfig,
    #[serde(default)]
    pub quality: QualityConfig,
}

fn default_max_context_tokens() -> usize {
    80_000
}

/// How many rig "turns" (model replies + tool round-trips) an agent may take
/// per user message before rig aborts the stream with `MaxTurnsError`. Rig's
/// own default (unset) is effectively ~1, far too low for a tool-calling
/// coding agent, so magai always sets an explicit cap.
fn default_max_turns() -> usize {
    25
}

fn default_true() -> bool {
    true
}

fn default_memory_snippets() -> usize {
    5
}

#[derive(Debug, Deserialize, Clone)]
pub struct MemoryConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub inject_context: bool,
    #[serde(default = "default_memory_snippets")]
    pub max_context_snippets: usize,
    pub db_path: Option<String>,
    /// When set, enables LLM-based fact extraction after each turn using this
    /// Ollama model name (e.g. "granite4:latest"). Disabled by default because
    /// it adds a background HTTP call per turn.
    pub extract_facts_model: Option<String>,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            inject_context: true,
            max_context_snippets: default_memory_snippets(),
            db_path: None,
            extract_facts_model: None,
        }
    }
}

/// Session/turn transcript + quality-rating tracking, for a future
/// fine-tuning export. Disabled by default since it persists full
/// conversation text.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct QualityConfig {
    #[serde(default)]
    pub enabled: bool,
    /// When set, enables LLM-as-judge rating of each finished turn using
    /// this Ollama model name (e.g. "granite4:latest"). Disabled by default
    /// because it adds a background HTTP call per turn.
    pub judge_model: Option<String>,
}

/// Per-turn snapshots of the working tree, kept in a shadow git repository
/// outside the project so the user's own repository is never written to.
#[derive(Debug, Deserialize, Clone)]
pub struct CheckpointsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Overrides the auto-detected project root (the directory that gets
    /// snapshotted). Rarely needed; `~` is expanded.
    pub root: Option<String>,
    /// Where the shadow repositories live. Defaults to
    /// `$XDG_DATA_HOME/magai/checkpoints`.
    pub store: Option<String>,
    /// How many checkpoints to keep before the oldest are pruned.
    #[serde(default = "default_keep")]
    pub keep: usize,
    /// Extra gitignore-style patterns excluded from snapshots.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Whether to apply the built-in exclude list (build output, dependency
    /// trees, editor junk). Matters most in projects with no `.gitignore`.
    #[serde(default = "default_true")]
    pub use_default_excludes: bool,
    /// Whether to copy the project's `.git/info/exclude` into the shadow
    /// repository, which cannot otherwise see it.
    #[serde(default = "default_true")]
    pub copy_project_exclude: bool,
    /// Rows shown by `/checkpoints`.
    #[serde(default = "default_max_list")]
    pub max_list: usize,
    /// Byte cap on the diff text `/diff` renders.
    #[serde(default = "default_diff_bytes")]
    pub diff_max_bytes: usize,
}

impl Default for CheckpointsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            root: None,
            store: None,
            keep: default_keep(),
            exclude: Vec::new(),
            use_default_excludes: true,
            copy_project_exclude: true,
            max_list: default_max_list(),
            diff_max_bytes: default_diff_bytes(),
        }
    }
}

fn default_keep() -> usize {
    200
}

fn default_max_list() -> usize {
    50
}

fn default_diff_bytes() -> usize {
    20_000
}

impl Config {
    /// Loads the config, falling back to `Config::default()` on any read or
    /// parse error. The second element of the tuple carries a human-readable
    /// warning describing such a fallback, so the caller can surface it
    /// somewhere visible (the TUI hides stderr, where this used to go).
    pub fn load() -> (Self, Option<String>) {
        let Some(path) = config_path().filter(|p| p.exists()) else {
            return (Self::default(), None);
        };
        let src = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                return (
                    Self::default(),
                    Some(format!("could not read config {:?}: {e}", path)),
                );
            }
        };
        match toml::from_str(&src) {
            Ok(cfg) => (cfg, None),
            Err(e) => (
                Self::default(),
                Some(format!("config parse error in {:?}: {e}", path)),
            ),
        }
    }

    pub fn find_named_model(&self, alias: &str) -> Option<(&NamedModel, &ProviderConfig)> {
        let nm = self.named_models.iter().find(|m| m.alias == alias)?;
        let pc = self.providers.get(&nm.provider)?;
        Some((nm, pc))
    }

    /// The first `[providers.*]` entry with `type = "ollama"`. Keys are sorted
    /// so a `HashMap`'s iteration order cannot make the choice flap between
    /// runs when several ollama providers are configured.
    pub fn ollama_provider(&self) -> Option<(&str, &ProviderConfig)> {
        let mut keys: Vec<&String> = self
            .providers
            .iter()
            .filter(|(_, pc)| pc.provider_type == ProviderType::Ollama)
            .map(|(k, _)| k)
            .collect();
        keys.sort();
        let key = keys.first()?;
        Some((key.as_str(), self.providers.get(*key)?))
    }

    /// Whether a bare model id may be read as an Ollama tag: either an ollama
    /// provider is configured, or nothing is configured at all (the
    /// zero-config case magai has always supported). A user who configured
    /// providers and deliberately left Ollama out never gets one invented.
    pub fn ollama_is_available(&self) -> bool {
        self.providers.is_empty() || self.ollama_provider().is_some()
    }

    /// Where Ollama lives, honouring config then environment.
    pub fn ollama_base_url(&self) -> String {
        ollama_base_url_from(
            self.ollama_provider()
                .and_then(|(_, pc)| pc.base_url.as_deref()),
            std::env::var(OLLAMA_BASE_URL_ENV).ok().as_deref(),
        )
    }

    /// Provider keys the UI may offer for `/provider`, sorted, with `"ollama"`
    /// included only when it is the implicit fallback rather than something
    /// the user configured away.
    pub fn provider_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.providers.keys().cloned().collect();
        if self.providers.is_empty() {
            names.push("ollama".to_string());
        }
        names.sort();
        names
    }

    /// Decides which model magai starts with, without touching the network.
    /// `env` is the environment lookup (`&|k| std::env::var(k).ok()` in
    /// production) so the whole precedence ladder is unit-testable without
    /// mutating the process environment.
    ///
    /// Precedence:
    ///
    /// 1. `default_model`, if set. The user was explicit, so a mismatch is an
    ///    error rather than a substitution — silent substitution is the bug
    ///    class this whole path exists to remove. A bare value is still read
    ///    as an Ollama tag when `ollama_is_available()`.
    /// 2. Otherwise the first `[[named_models]]` entry that is actually
    ///    usable: its provider key resolves, and for a hosted provider its
    ///    `api_key_env` is present in `env`. This is what makes one shared
    ///    config work on machines that export different keys.
    /// 3. Otherwise a provider is *identified* but no model is chosen, and
    ///    [`StartupChoice::NeedsModel`] is returned so the caller can list
    ///    that provider's catalogue and stop. In order: a configured provider
    ///    whose credentials are present, then an [`AUTODETECT`] row whose env
    ///    var is set, then Ollama.
    ///
    /// Rung 3 never invents a model id. A baked-in default goes stale, may not
    /// exist on the account, and silently picking one from a catalogue is how
    /// you end up running an image or embedding model as a coding agent.
    pub fn startup_choice(
        &self,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<StartupChoice, String> {
        if let Some(alias) = self.default_model.as_deref() {
            return self
                .startup_from_default_model(alias)
                .map(StartupChoice::Model);
        }

        if !self.named_models.is_empty() {
            if let Some(m) = self
                .named_models
                .iter()
                .find_map(|nm| self.usable_named_model(nm, env))
            {
                return Ok(StartupChoice::Model(m));
            }
            return Err(self.no_usable_named_model_message());
        }

        // Providers configured but no `[[named_models]]` to pick from: offer
        // the first usable one's catalogue. Sorted so the choice is stable.
        let mut keys: Vec<&String> = self.providers.keys().collect();
        keys.sort();
        for key in keys {
            let pc = &self.providers[key];
            let usable = pc.provider_type == ProviderType::Ollama
                || pc
                    .api_key_env
                    .as_deref()
                    .and_then(env)
                    .is_some_and(|v| !v.is_empty());
            if usable {
                return Ok(StartupChoice::NeedsModel(UnresolvedProvider {
                    label: key.clone(),
                    provider: pc.clone(),
                    detected_via: format!("provider {key:?} is configured"),
                }));
            }
        }

        for (provider_type, key_env) in AUTODETECT {
            if env(key_env).is_some_and(|v| !v.is_empty()) {
                return Ok(StartupChoice::NeedsModel(UnresolvedProvider {
                    label: provider_type.as_str().to_string(),
                    provider: ProviderConfig {
                        provider_type: *provider_type,
                        api_key_env: Some((*key_env).to_string()),
                        base_url: None,
                        api: OpenAIApi::default(),
                    },
                    detected_via: format!("{key_env} is set"),
                }));
            }
        }

        Ok(StartupChoice::NeedsModel(UnresolvedProvider {
            label: "ollama".to_string(),
            provider: ProviderConfig {
                provider_type: ProviderType::Ollama,
                api_key_env: None,
                base_url: self
                    .ollama_provider()
                    .and_then(|(_, pc)| pc.base_url.clone()),
                api: OpenAIApi::default(),
            },
            detected_via: "no provider API keys were found, so magai fell back to ollama"
                .to_string(),
        }))
    }

    fn startup_from_default_model(&self, alias: &str) -> Result<StartupModel, String> {
        if let Some((nm, pc)) = self.find_named_model(alias) {
            return Ok(StartupModel {
                display: alias.to_string(),
                alias: Some(alias.to_string()),
                provider_type: pc.provider_type,
                model: nm.model.clone(),
                api_key_env: pc.api_key_env.clone(),
                base_url: pc.base_url.clone(),
                api: nm.api(pc),
            });
        }
        // A named model whose `provider` key has no `[providers.*]` entry also
        // lands here. Say so specifically — it used to become an Ollama tag.
        if let Some(nm) = self.named_models.iter().find(|m| m.alias == alias) {
            return Err(format!(
                "named model {alias:?} references provider {:?}, which has no \
                 [providers.{}] entry in the config",
                nm.provider, nm.provider
            ));
        }
        if self.ollama_is_available() {
            return Ok(StartupModel {
                display: alias.to_string(),
                alias: None,
                provider_type: ProviderType::Ollama,
                model: alias.to_string(),
                api_key_env: None,
                base_url: self
                    .ollama_provider()
                    .and_then(|(_, pc)| pc.base_url.clone()),
                api: OpenAIApi::default(),
            });
        }
        Err(format!(
            "default_model {alias:?} names no [[named_models]] entry. Configured \
             aliases: {}. Add an entry for it, or set default_model to one of \
             those.",
            self.alias_list()
        ))
    }

    /// `Some` when this entry's provider resolves and its credentials are
    /// present; `None` when it should be skipped in favour of a later entry.
    fn usable_named_model(
        &self,
        nm: &NamedModel,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Option<StartupModel> {
        let pc = self.providers.get(&nm.provider)?;
        let credentialed = match pc.provider_type {
            ProviderType::Ollama => true,
            _ => pc
                .api_key_env
                .as_deref()
                .and_then(env)
                .is_some_and(|v| !v.is_empty()),
        };
        credentialed.then(|| StartupModel {
            display: nm.alias.clone(),
            alias: Some(nm.alias.clone()),
            provider_type: pc.provider_type,
            model: nm.model.clone(),
            api_key_env: pc.api_key_env.clone(),
            base_url: pc.base_url.clone(),
            api: nm.api(pc),
        })
    }

    fn alias_list(&self) -> String {
        if self.named_models.is_empty() {
            return "(none)".to_string();
        }
        self.named_models
            .iter()
            .map(|m| m.alias.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn no_usable_named_model_message(&self) -> String {
        let details: Vec<String> = self
            .named_models
            .iter()
            .map(|nm| match self.providers.get(&nm.provider) {
                None => format!("{}: provider {:?} is not configured", nm.alias, nm.provider),
                Some(pc) => match pc.api_key_env.as_deref() {
                    Some(var) => format!("{}: {var} is not set", nm.alias),
                    None => format!(
                        "{}: provider {:?} has no api_key_env",
                        nm.alias, nm.provider
                    ),
                },
            })
            .collect();
        format!(
            "no usable model: every [[named_models]] entry is missing its \
             credentials or provider ({})",
            details.join("; ")
        )
    }
}

/// Expands a leading `~/` against `$HOME`. Any other path is returned as-is.
pub fn expand_tilde(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => match std::env::var("HOME") {
            Ok(home) => format!("{}/{rest}", home.trim_end_matches('/')),
            Err(_) => path.to_string(),
        },
        None => path.to_string(),
    }
}

/// `~/.config/magai/config.toml` (honouring `XDG_CONFIG_HOME`).
pub fn config_path() -> Option<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok()?;
    Some(base.join("magai").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::PermissionMode;

    const EXAMPLE: &str = include_str!("../docs/config.example.toml");

    #[test]
    fn stdio_server_transport() {
        let cfg: Config = toml::from_str(
            r#"
            [[mcp_servers]]
            name = "fs"
            command = "npx"
            args = ["-y", "server-filesystem", "/tmp"]
            [mcp_servers.env]
            TOKEN = "abc"
            "#,
        )
        .expect("stdio server should parse");
        let server = &cfg.mcp_servers[0];
        assert!(!server.trusted, "servers are gated unless marked trusted");
        match server.transport().expect("stdio transport") {
            McpTransport::Stdio { command, args, env } => {
                assert_eq!(command, "npx");
                assert_eq!(args.len(), 3);
                assert_eq!(env.get("TOKEN").map(String::as_str), Some("abc"));
            }
            other => panic!("expected stdio, got {other:?}"),
        }
    }

    #[test]
    fn http_server_transport_wins_over_command() {
        let cfg: Config = toml::from_str(
            r#"
            [[mcp_servers]]
            name = "remote"
            command = "ignored"
            url = "https://mcp.example.com/mcp"
            bearer_token = "tok"
            trusted = true
            [mcp_servers.headers]
            X-Tenant = "acme"
            "#,
        )
        .expect("remote server should parse");
        let server = &cfg.mcp_servers[0];
        assert!(server.trusted);
        match server.transport().expect("http transport") {
            McpTransport::Http {
                url,
                headers,
                bearer_token,
            } => {
                assert_eq!(url, "https://mcp.example.com/mcp");
                assert_eq!(bearer_token.as_deref(), Some("tok"));
                assert_eq!(headers.get("X-Tenant").map(String::as_str), Some("acme"));
            }
            other => panic!("expected http, got {other:?}"),
        }
    }

    #[test]
    fn server_without_command_or_url_is_rejected() {
        let cfg: Config = toml::from_str(
            r#"
            [[mcp_servers]]
            name = "broken"
            "#,
        )
        .expect("entry should parse");
        let err = cfg.mcp_servers[0].transport().unwrap_err();
        assert!(
            err.contains("broken"),
            "error should name the server: {err}"
        );
    }

    #[test]
    fn expand_env_substitutes_and_tolerates_junk() {
        std::env::set_var("MAGAI_TEST_TOKEN", "s3cret");
        assert_eq!(expand_env("Bearer ${MAGAI_TEST_TOKEN}"), "Bearer s3cret");
        assert_eq!(expand_env("${MAGAI_TEST_UNSET_VAR}!"), "!");
        assert_eq!(expand_env("plain"), "plain");
        // an unterminated `${` is left alone rather than eating the rest
        assert_eq!(expand_env("a ${oops"), "a ${oops");
    }

    /// Builds an environment lookup from a list of pairs, so the precedence
    /// ladder can be tested without mutating the real process environment.
    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| (*v).to_string())
        }
    }

    /// Unwraps a resolution that should have produced a concrete model.
    fn model_of(c: StartupChoice) -> StartupModel {
        match c {
            StartupChoice::Model(m) => m,
            StartupChoice::NeedsModel(u) => {
                panic!(
                    "expected a model, got a prompt to pick one from {:?}",
                    u.label
                )
            }
        }
    }

    /// Unwraps a resolution that should have asked the user to pick.
    fn needs_of(c: StartupChoice) -> UnresolvedProvider {
        match c {
            StartupChoice::NeedsModel(u) => u,
            StartupChoice::Model(m) => panic!("expected a prompt to pick, got {:?}", m.display),
        }
    }

    const TWO_PROVIDERS: &str = r#"
        [providers.ollama]
        type = "ollama"

        [providers.anthropic]
        type = "anthropic"
        api_key_env = "ANTHROPIC_API_KEY"

        [[named_models]]
        alias = "local"
        provider = "ollama"
        model = "granite4:latest"

        [[named_models]]
        alias = "sonnet"
        provider = "anthropic"
        model = "claude-sonnet-5"
    "#;

    #[test]
    fn ollama_base_url_prefers_config_then_env_then_default() {
        assert_eq!(
            ollama_base_url_from(Some("http://a:1"), Some("http://b:2")),
            "http://a:1"
        );
        assert_eq!(ollama_base_url_from(None, Some("http://b:2")), "http://b:2");
        assert_eq!(ollama_base_url_from(None, None), OLLAMA_DEFAULT_BASE_URL);
        // an empty value is not a choice
        assert_eq!(
            ollama_base_url_from(Some("  "), None),
            OLLAMA_DEFAULT_BASE_URL
        );
    }

    #[test]
    fn ollama_base_url_trims_trailing_slash() {
        // rig's `build_uri` appends "/" + path unconditionally, so a trailing
        // slash here would produce "http://h:11434//api/tags".
        assert_eq!(
            ollama_base_url_from(Some("http://h:11434/"), None),
            "http://h:11434"
        );
    }

    #[test]
    fn ollama_is_available_only_when_implicit_or_configured() {
        let empty: Config = toml::from_str("").unwrap();
        assert!(empty.ollama_is_available(), "zero-config implies ollama");

        let with_ollama: Config = toml::from_str(TWO_PROVIDERS).unwrap();
        assert!(with_ollama.ollama_is_available());

        let hosted_only: Config = toml::from_str(
            r#"
            [providers.anthropic]
            type = "anthropic"
            api_key_env = "ANTHROPIC_API_KEY"
            "#,
        )
        .unwrap();
        assert!(
            !hosted_only.ollama_is_available(),
            "a config that deliberately omits ollama should not get one invented"
        );
    }

    #[test]
    fn provider_names_omits_implicit_ollama_when_providers_configured() {
        let hosted_only: Config = toml::from_str(
            r#"
            [providers.anthropic]
            type = "anthropic"
            "#,
        )
        .unwrap();
        assert_eq!(hosted_only.provider_names(), vec!["anthropic".to_string()]);

        let empty: Config = toml::from_str("").unwrap();
        assert_eq!(empty.provider_names(), vec!["ollama".to_string()]);
    }

    #[test]
    fn startup_prefers_explicit_default_model_alias() {
        let mut cfg: Config = toml::from_str(TWO_PROVIDERS).unwrap();
        cfg.default_model = Some("sonnet".into());
        let m = model_of(cfg.startup_choice(&env_of(&[])).unwrap());
        assert_eq!(m.alias.as_deref(), Some("sonnet"));
        assert_eq!(m.provider_type, ProviderType::Anthropic);
        assert_eq!(m.model, "claude-sonnet-5");
        // an explicit alias wins even with no credentials present — the
        // preflight reports that separately rather than substituting
        assert_eq!(m.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn startup_unknown_default_model_is_an_ollama_tag_when_ollama_available() {
        let mut cfg: Config = toml::from_str(TWO_PROVIDERS).unwrap();
        cfg.default_model = Some("qwen3:8b".into());
        let m = model_of(cfg.startup_choice(&env_of(&[])).unwrap());
        assert_eq!(m.provider_type, ProviderType::Ollama);
        assert_eq!(m.model, "qwen3:8b");
        assert!(m.alias.is_none());
    }

    #[test]
    fn startup_unknown_default_model_errors_when_ollama_absent() {
        let cfg: Config = toml::from_str(
            r#"
            default_model = "typo"

            [providers.anthropic]
            type = "anthropic"
            api_key_env = "ANTHROPIC_API_KEY"

            [[named_models]]
            alias = "sonnet"
            provider = "anthropic"
            model = "claude-sonnet-5"
            "#,
        )
        .unwrap();
        let err = cfg.startup_choice(&env_of(&[])).unwrap_err();
        assert!(err.contains("typo"), "error should name the alias: {err}");
        assert!(
            err.contains("sonnet"),
            "error should list real aliases: {err}"
        );
    }

    #[test]
    fn startup_named_model_with_missing_provider_errors() {
        let cfg: Config = toml::from_str(
            r#"
            default_model = "sonnet"

            [providers.anthropic]
            type = "anthropic"
            api_key_env = "ANTHROPIC_API_KEY"

            [[named_models]]
            alias = "sonnet"
            provider = "anthropik"
            model = "claude-sonnet-5"
            "#,
        )
        .unwrap();
        // This used to silently become an Ollama tag named "sonnet".
        let err = cfg.startup_choice(&env_of(&[])).unwrap_err();
        assert!(
            err.contains("anthropik"),
            "error should name the provider: {err}"
        );
    }

    #[test]
    fn startup_falls_back_to_first_credentialed_named_model() {
        let cfg: Config = toml::from_str(
            r#"
            [providers.openai]
            type = "openai"
            api_key_env = "OPENAI_API_KEY"

            [providers.groq]
            type = "groq"
            api_key_env = "GROQ_API_KEY"

            [[named_models]]
            alias = "gpt"
            provider = "openai"
            model = "gpt-4o"

            [[named_models]]
            alias = "fast"
            provider = "groq"
            model = "llama-3.3-70b-versatile"
            "#,
        )
        .unwrap();
        // The first entry is skipped because its key is absent, so one shared
        // config works on a machine that only exports GROQ_API_KEY.
        let m = model_of(
            cfg.startup_choice(&env_of(&[("GROQ_API_KEY", "k")]))
                .unwrap(),
        );
        assert_eq!(m.alias.as_deref(), Some("fast"));
    }

    #[test]
    fn startup_errors_when_named_models_exist_but_no_credentials() {
        let cfg: Config = toml::from_str(
            r#"
            [providers.openai]
            type = "openai"
            api_key_env = "OPENAI_API_KEY"

            [[named_models]]
            alias = "gpt"
            provider = "openai"
            model = "gpt-4o"
            "#,
        )
        .unwrap();
        let err = cfg.startup_choice(&env_of(&[])).unwrap_err();
        assert!(
            err.contains("OPENAI_API_KEY"),
            "error should name the var: {err}"
        );
    }

    #[test]
    fn startup_autodetects_provider_from_env_in_priority_order() {
        let cfg: Config = toml::from_str("").unwrap();
        let both = needs_of(
            cfg.startup_choice(&env_of(&[
                ("OPENAI_API_KEY", "k"),
                ("ANTHROPIC_API_KEY", "k"),
            ]))
            .unwrap(),
        );
        assert_eq!(both.provider.provider_type, ProviderType::Anthropic);

        let groq = needs_of(
            cfg.startup_choice(&env_of(&[("GROQ_API_KEY", "k")]))
                .unwrap(),
        );
        assert_eq!(groq.provider.provider_type, ProviderType::Groq);
        assert!(
            groq.detected_via.contains("GROQ_API_KEY"),
            "the message should name what was detected: {}",
            groq.detected_via
        );
    }

    #[test]
    fn startup_never_invents_a_model_id() {
        // A detected provider says which catalogue to list, never which model
        // to run: a baked-in id goes stale and may not exist on the account.
        let cfg: Config = toml::from_str("").unwrap();
        let u = needs_of(
            cfg.startup_choice(&env_of(&[("ANTHROPIC_API_KEY", "k")]))
                .unwrap(),
        );
        assert_eq!(u.label, "anthropic");
        assert_eq!(u.provider.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn startup_offers_a_configured_provider_when_no_named_models() {
        let cfg: Config = toml::from_str(
            r#"
            [providers.openrouter]
            type = "openai"
            api_key_env = "OPENROUTER_API_KEY"
            "#,
        )
        .unwrap();
        let u = needs_of(
            cfg.startup_choice(&env_of(&[("OPENROUTER_API_KEY", "k")]))
                .unwrap(),
        );
        assert_eq!(u.label, "openrouter");
    }

    #[test]
    fn startup_falls_back_to_ollama_last_without_choosing_a_tag() {
        let cfg: Config = toml::from_str("").unwrap();
        let u = needs_of(cfg.startup_choice(&env_of(&[])).unwrap());
        assert_eq!(u.provider.provider_type, ProviderType::Ollama);
        assert!(
            u.detected_via.contains("no provider API keys"),
            "{}",
            u.detected_via
        );
    }

    #[test]
    fn parses_example_config() {
        let cfg: Config = toml::from_str(EXAMPLE).expect("example config should parse");

        assert_eq!(cfg.default_model.as_deref(), Some("local"));
        assert_eq!(cfg.permission_mode, PermissionMode::AskDangerous);
        assert_eq!(cfg.max_context_tokens, 80_000);
        assert_eq!(cfg.max_turns, 25);

        // The [checkpoints] block is commented out in the example, so this
        // pins the defaults a user gets without configuring anything.
        assert!(cfg.checkpoints.enabled);
        assert!(cfg.checkpoints.use_default_excludes);
        assert_eq!(cfg.checkpoints.keep, 200);

        assert_eq!(cfg.providers.len(), 3);
        let openai = cfg.providers.get("openai").expect("openai provider");
        assert_eq!(openai.provider_type, ProviderType::OpenAI);
        assert_eq!(openai.api_key_env.as_deref(), Some("OPENAI_API_KEY"));

        assert_eq!(cfg.named_models.len(), 3);
        let (nm, pc) = cfg.find_named_model("gpt4o").expect("gpt4o resolves");
        assert_eq!(nm.model, "gpt-4o");
        assert_eq!(pc.provider_type, ProviderType::OpenAI);
    }

    #[test]
    fn resolve_system_prompt_prefers_inline_over_file() {
        let nm = NamedModel {
            alias: "a".to_string(),
            provider: "p".to_string(),
            model: "m".to_string(),
            system_prompt: Some("inline prompt".to_string()),
            system_prompt_file: Some("/nonexistent/path/does-not-matter".to_string()),
            api: None,
        };
        assert_eq!(
            nm.resolve_system_prompt().unwrap().as_deref(),
            Some("inline prompt")
        );
    }

    #[test]
    fn resolve_system_prompt_reads_file_when_no_inline() {
        let dir =
            std::env::temp_dir().join(format!("magai-test-system-prompt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prompt.md");
        std::fs::write(&path, "from file").unwrap();

        let nm = NamedModel {
            alias: "a".to_string(),
            provider: "p".to_string(),
            model: "m".to_string(),
            system_prompt: None,
            system_prompt_file: Some(path.to_string_lossy().to_string()),
            api: None,
        };
        assert_eq!(
            nm.resolve_system_prompt().unwrap().as_deref(),
            Some("from file")
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn named_model_api_overrides_provider_api() {
        let config: Config = toml::from_str(
            r#"
            [providers.proxy]
            type = "openai"
            base_url = "http://localhost:4000/v1"
            api = "responses"

            [[named_models]]
            alias = "inherits"
            provider = "proxy"
            model = "a"

            [[named_models]]
            alias = "overrides"
            provider = "proxy"
            model = "b"
            api = "chat"
            "#,
        )
        .unwrap();
        let (nm, pc) = config.find_named_model("inherits").unwrap();
        assert_eq!(nm.api(pc), OpenAIApi::Responses);
        let (nm, pc) = config.find_named_model("overrides").unwrap();
        assert_eq!(nm.api(pc), OpenAIApi::Chat);
    }

    #[test]
    fn provider_api_defaults_to_chat() {
        let pc: ProviderConfig = toml::from_str(r#"type = "openai""#).unwrap();
        assert_eq!(pc.api, OpenAIApi::Chat);
    }

    #[test]
    fn resolve_system_prompt_none_when_unset() {
        let nm = NamedModel {
            alias: "a".to_string(),
            provider: "p".to_string(),
            model: "m".to_string(),
            system_prompt: None,
            system_prompt_file: None,
            api: None,
        };
        assert_eq!(nm.resolve_system_prompt().unwrap(), None);
    }

    #[test]
    fn malformed_toml_is_not_a_valid_config() {
        let result: Result<Config, _> = toml::from_str("default_model = [this is not valid");
        assert!(result.is_err());
    }

    #[test]
    fn empty_toml_uses_defaults() {
        let cfg: Config = toml::from_str("").expect("empty config parses");
        assert_eq!(cfg.named_models.len(), 0);
        assert_eq!(cfg.providers.len(), 0);
        assert_eq!(cfg.default_model, None);
        assert_eq!(cfg.permission_mode, PermissionMode::AskDangerous);
        assert_eq!(cfg.max_context_tokens, 80_000);
        assert_eq!(cfg.max_turns, 25);
    }
}
