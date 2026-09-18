use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ProviderType {
    Ollama,
    OpenAI,
    Anthropic,
    Groq,
    Gemini,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ProviderConfig {
    #[serde(rename = "type")]
    pub provider_type: ProviderType,
    pub api_key_env: Option<String>,
    pub base_url: Option<String>,
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
}

impl NamedModel {
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
        };
        assert_eq!(
            nm.resolve_system_prompt().unwrap().as_deref(),
            Some("from file")
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_system_prompt_none_when_unset() {
        let nm = NamedModel {
            alias: "a".to_string(),
            provider: "p".to_string(),
            model: "m".to_string(),
            system_prompt: None,
            system_prompt_file: None,
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
