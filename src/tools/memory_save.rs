use std::sync::Arc;

use rig::completion::request::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

use crate::memory::{extract::save_note, MemoryDb};

#[derive(Deserialize)]
pub struct MemorySaveArgs {
    pub content: String,
}

pub struct MemorySave {
    db: Arc<MemoryDb>,
    session_alias: String,
}

impl MemorySave {
    pub fn new(db: Arc<MemoryDb>, session_alias: String) -> Self {
        Self { db, session_alias }
    }
}

impl Tool for MemorySave {
    const NAME: &'static str = "memory_save";
    type Error = std::io::Error;
    type Args = MemorySaveArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "memory_save".to_string(),
            description: "Save a note to persistent memory. \
                Use this to record facts, decisions, or context that should be \
                recalled in future sessions — e.g. project conventions, user preferences, \
                or conclusions reached during this conversation."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "The fact or note to save. Be concise and specific."
                    }
                },
                "required": ["content"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        save_note(&self.db, &args.content, &self.session_alias);
        Ok(format!("Saved to memory: {}", args.content))
    }
}
