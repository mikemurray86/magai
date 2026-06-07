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
    #[serde(default)]
    pub hooks: Vec<crate::hooks::HookConfig>,
}

fn default_max_context_tokens() -> usize {
    80_000
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
