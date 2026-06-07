use rig::completion::request::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;

#[derive(Deserialize)]
pub struct ListDirectoryArgs {
    pub path: Option<String>,
    #[serde(default)]
    pub show_hidden: bool,
}

#[derive(Deserialize, Serialize)]
pub struct ListDirectory;

impl Tool for ListDirectory {
    const NAME: &'static str = "list_directory";
    type Error = std::io::Error;
    type Args = ListDirectoryArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "list_directory".to_string(),
            description: "List the immediate contents of a directory with type (file/dir) and size. Use find_files for recursive searches.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "directory to list (defaults to current directory)" },
                    "show_hidden": { "type": "boolean", "description": "include hidden entries (names starting with '.'), default false" }
                }
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let dir = match args.path {
            Some(p) => PathBuf::from(p),
            None => std::env::current_dir()?,
        };
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !args.show_hidden && name.starts_with('.') {
                continue;
            }
            let metadata = entry.metadata()?;
            let kind = if metadata.is_dir() { "dir" } else { "file" };
            entries.push(json!({
                "name": name,
                "kind": kind,
                "size_bytes": metadata.len()
            }));
        }
        // dirs first, then files; alphabetical within each group
        entries.sort_by(|a, b| {
            let ka = a["kind"].as_str().unwrap_or("");
            let kb = b["kind"].as_str().unwrap_or("");
            let na = a["name"].as_str().unwrap_or("");
            let nb = b["name"].as_str().unwrap_or("");
            kb.cmp(ka).then(na.cmp(nb))
        });
        Ok(json!({ "path": dir.to_string_lossy(), "entries": entries }).to_string())
    }
}
