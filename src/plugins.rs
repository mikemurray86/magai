use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::config::McpServerConfig;
use crate::hooks::HookConfig;
use crate::skills::Skill;

#[derive(Debug, Clone, Deserialize)]
pub struct McpServerDef {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub bearer_token: Option<String>,
    #[serde(default)]
    pub trusted: bool,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SkillRef {
    pub path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub mcp_server: Option<McpServerDef>,
    #[serde(default)]
    pub skills: Vec<SkillRef>,
    #[serde(default)]
    pub hooks: Vec<HookConfig>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PluginScope {
    Global,
    Project,
}

#[derive(Debug, Clone)]
pub struct Plugin {
    pub manifest: PluginManifest,
    pub dir: PathBuf,
    pub scope: PluginScope,
}

fn global_plugins_dir() -> Option<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok()?;
    Some(base.join("magai").join("plugins"))
}

fn load_from_dir(dir: &PathBuf, scope: PluginScope, map: &mut HashMap<String, Plugin>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let manifest_path = path.join("plugin.toml");
        let Ok(content) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        match toml::from_str::<PluginManifest>(&content) {
            Ok(manifest) => {
                let name = manifest.name.clone();
                map.insert(
                    name,
                    Plugin {
                        manifest,
                        dir: path,
                        scope: scope.clone(),
                    },
                );
            }
            Err(e) => {
                tracing::warn!("Plugin at {:?}: failed to parse plugin.toml: {e}", path);
            }
        }
    }
}

/// Discovers plugins from global (`~/.config/magai/plugins/`) and project-local
/// (`.magai/plugins/`) directories. Project-local plugins override global ones by name.
pub fn discover() -> Vec<Plugin> {
    let mut map: HashMap<String, Plugin> = HashMap::new();

    if let Some(dir) = global_plugins_dir() {
        load_from_dir(&dir, PluginScope::Global, &mut map);
    }
    load_from_dir(
        &PathBuf::from(".magai/plugins"),
        PluginScope::Project,
        &mut map,
    );

    let mut result: Vec<Plugin> = map.into_values().collect();
    result.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
    result
}

pub fn extract_mcp_configs(plugins: &[Plugin]) -> Vec<McpServerConfig> {
    plugins
        .iter()
        .filter_map(|p| {
            p.manifest.mcp_server.as_ref().map(|s| McpServerConfig {
                name: p.manifest.name.clone(),
                command: s.command.clone(),
                args: s.args.clone(),
                env: s.env.clone(),
                url: s.url.clone(),
                headers: s.headers.clone(),
                bearer_token: s.bearer_token.clone(),
                trusted: s.trusted,
                timeout_secs: s.timeout_secs,
            })
        })
        .collect()
}

pub fn extract_skills(plugins: &[Plugin]) -> Vec<Skill> {
    let mut skills = Vec::new();
    for plugin in plugins {
        for skill_ref in &plugin.manifest.skills {
            let path = plugin.dir.join(&skill_ref.path);
            let Ok(content) = std::fs::read_to_string(&path) else {
                tracing::warn!(
                    "Plugin '{}': could not read skill at {:?}",
                    plugin.manifest.name,
                    path
                );
                continue;
            };
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
                .unwrap_or_else(|| skill_ref.path.clone());
            let description = content
                .lines()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.trim_start_matches('#').trim().to_string())
                .unwrap_or_default();
            skills.push(Skill {
                name,
                description,
                content,
            });
        }
    }
    skills
}

pub fn extract_hooks(plugins: &[Plugin]) -> Vec<HookConfig> {
    plugins
        .iter()
        .flat_map(|p| p.manifest.hooks.iter().cloned())
        .collect()
}

pub fn summary(plugins: &[Plugin]) -> String {
    if plugins.is_empty() {
        return "No plugins loaded.\n\nInstall plugins in:\n  ~/.config/magai/plugins/<name>/  (global)\n  .magai/plugins/<name>/           (project-local)\n\nEach plugin directory needs a plugin.toml manifest.".to_string();
    }
    let mut lines = Vec::new();
    for plugin in plugins {
        let scope = match plugin.scope {
            PluginScope::Global => "global",
            PluginScope::Project => "project",
        };
        let ver = plugin
            .manifest
            .version
            .as_deref()
            .map(|v| format!(" v{v}"))
            .unwrap_or_default();
        lines.push(format!("● {} [{}]{}", plugin.manifest.name, scope, ver));
        if let Some(desc) = &plugin.manifest.description {
            lines.push(format!("  {desc}"));
        }
        if let Some(mcp) = &plugin.manifest.mcp_server {
            let target = mcp
                .url
                .clone()
                .or_else(|| mcp.command.clone())
                .unwrap_or_else(|| "<no command or url>".to_string());
            lines.push(format!("  mcp: {target}"));
        }
        if !plugin.manifest.skills.is_empty() {
            let names: Vec<_> = plugin
                .manifest
                .skills
                .iter()
                .map(|s| s.path.as_str())
                .collect();
            lines.push(format!("  skills: {}", names.join(", ")));
        }
        if !plugin.manifest.hooks.is_empty() {
            let events: Vec<_> = plugin
                .manifest
                .hooks
                .iter()
                .map(|h| h.event.as_str())
                .collect();
            lines.push(format!("  hooks: {}", events.join(", ")));
        }
    }
    lines.join("\n")
}
