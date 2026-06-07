use std::collections::HashMap;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    SessionStart,
    SessionStop,
    PreToolCall,
    PostToolCall,
    AgentResponse,
}

impl HookEvent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::SessionStop => "session_stop",
            Self::PreToolCall => "pre_tool_call",
            Self::PostToolCall => "post_tool_call",
            Self::AgentResponse => "agent_response",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct HookConfig {
    pub event: HookEvent,
    pub command: String,
}

pub struct HookRunner {
    hooks: Vec<HookConfig>,
}

impl HookRunner {
    pub fn new(hooks: Vec<HookConfig>) -> Self {
        Self { hooks }
    }

    /// Fires all hooks matching `event`, passing `env` as additional environment variables.
    /// Each hook runs as a detached child process (fire-and-forget).
    pub fn fire(&self, event: HookEvent, env: HashMap<String, String>) {
        for hook in &self.hooks {
            if hook.event == event {
                let mut cmd = std::process::Command::new("sh");
                cmd.arg("-c").arg(&hook.command);
                for (k, v) in &env {
                    cmd.env(k, v);
                }
                // spawn() starts the child without waiting; errors are silently ignored
                let _ = cmd.spawn();
            }
        }
    }
}
