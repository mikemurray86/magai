use std::sync::Arc;

use rig::completion::request::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

use crate::memory::{retrieval::query_with_neighbors, MemoryDb};

#[derive(Deserialize)]
pub struct MemoryQueryArgs {
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    10
}

pub struct MemoryQuery {
    db: Arc<MemoryDb>,
}

impl MemoryQuery {
    pub fn new(db: Arc<MemoryDb>) -> Self {
        Self { db }
    }
}

impl Tool for MemoryQuery {
    const NAME: &'static str = "memory_query";
    type Error = std::io::Error;
    type Args = MemoryQueryArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "memory_query".to_string(),
            description: "Search the agent's persistent memory graph. \
                Returns files, concepts, and commands recorded from previous sessions, \
                plus their co-occurring related entities. \
                Use this when you need to recall what was worked on before."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "File names, identifiers, or keywords to look up in memory"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of direct matches (default 10)"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let lines = query_with_neighbors(&self.db, &args.query, args.limit);
        if lines.is_empty() {
            Ok("No memory entries found for that query.".to_string())
        } else {
            Ok(lines.join("\n"))
        }
    }
}
