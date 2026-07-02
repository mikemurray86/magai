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
}

#[derive(Debug, Deserialize, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
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
    #[serde(default = "default_true")]
    pub git_checkpointing: bool,
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

fn config_path() -> Option<PathBuf> {
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
    fn parses_example_config() {
        let cfg: Config = toml::from_str(EXAMPLE).expect("example config should parse");

        assert_eq!(cfg.default_model.as_deref(), Some("local"));
        assert_eq!(cfg.permission_mode, PermissionMode::AskDangerous);
        assert_eq!(cfg.max_context_tokens, 80_000);
        assert_eq!(cfg.max_turns, 25);

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
