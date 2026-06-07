use rig::completion::request::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Deserialize)]
pub struct ReadFileRangeArgs {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Deserialize, Serialize)]
pub struct ReadFileRange;

impl Tool for ReadFileRange {
    const NAME: &'static str = "read_file_range";
    type Error = std::io::Error;
    type Args = ReadFileRangeArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "read_file_range".to_string(),
            description: "Read a specific line range from a file (1-indexed, inclusive). More efficient than read_file for large files when you know which lines you need.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "path to the file" },
                    "start_line": { "type": "integer", "description": "first line to read (1-indexed)" },
                    "end_line": { "type": "integer", "description": "last line to read (inclusive)" }
                },
                "required": ["path", "start_line", "end_line"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let contents = std::fs::read_to_string(&args.path)?;
        let all_lines: Vec<&str> = contents.lines().collect();
        let total = all_lines.len();
        let start = args.start_line.saturating_sub(1);
        let end = args.end_line.min(total);
        let text = if start < total {
            all_lines[start..end].join("\n")
        } else {
            String::new()
        };
        Ok(json!({
            "lines": text,
            "total_lines": total,
            "start": start + 1,
            "end": end
        })
        .to_string())
    }
}
