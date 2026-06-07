use rig::completion::request::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;

fn default_create_dirs() -> bool {
    true
}

#[derive(Deserialize)]
pub struct WriteFileArgs {
    pub path: String,
    pub content: String,
    #[serde(default = "default_create_dirs")]
    pub create_dirs: bool,
}

#[derive(Deserialize, Serialize)]
pub struct WriteFile;

impl Tool for WriteFile {
    const NAME: &'static str = "write_file";
    type Error = std::io::Error;
    type Args = WriteFileArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "write_file".to_string(),
            description: "Create or overwrite a file with the given content. Prefer this over shell_command for writing files.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "absolute or relative path to the file" },
                    "content": { "type": "string", "description": "full content to write to the file" },
                    "create_dirs": { "type": "boolean", "description": "create parent directories if missing (default true)" }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let path = PathBuf::from(&args.path);
        if args.create_dirs {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
        }
        let bytes = args.content.len();
        std::fs::write(&path, &args.content)?;
        let abs = path.canonicalize().unwrap_or(path);
        Ok(json!({ "bytes_written": bytes, "path": abs.to_string_lossy() }).to_string())
    }
}
