use rig::completion::request::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;
use walkdir::WalkDir;

fn default_max_depth() -> usize {
    10
}
fn default_max_results() -> usize {
    200
}

#[derive(Deserialize)]
pub struct FindFilesArgs {
    pub path: Option<String>,
    pub pattern: Option<String>,
    #[serde(default = "default_max_depth")]
    pub max_depth: usize,
    #[serde(default = "default_max_results")]
    pub max_results: usize,
    #[serde(default)]
    pub include_hidden: bool,
}

#[derive(Deserialize, Serialize)]
pub struct FindFiles;

impl Tool for FindFiles {
    const NAME: &'static str = "find_files";
    type Error = std::io::Error;
    type Args = FindFilesArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "find_files".to_string(),
            description: "List files matching a glob pattern within a directory tree. Use list_directory for a single directory, grep_search to search by content.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "root directory to search (defaults to current directory)" },
                    "pattern": { "type": "string", "description": "glob pattern to match filenames or relative paths, e.g. '*.rs' or '**/*.toml'" },
                    "max_depth": { "type": "integer", "description": "maximum directory depth to recurse (default 10)" },
                    "max_results": { "type": "integer", "description": "maximum number of results (default 200)" },
                    "include_hidden": { "type": "boolean", "description": "include hidden files and directories (default false)" }
                }
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let root = match args.path {
            Some(ref p) => PathBuf::from(p),
            None => std::env::current_dir()?,
        };

        let pattern = args.pattern.as_deref();
        let mut files: Vec<String> = Vec::new();
        let mut truncated = false;

        for entry in WalkDir::new(&root)
            .max_depth(args.max_depth)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            // Skip hidden files/dirs unless requested
            if !args.include_hidden {
                let hidden = entry
                    .path()
                    .components()
                    .any(|c| c.as_os_str().to_string_lossy().starts_with('.'));
                if hidden {
                    continue;
                }
            }

            // Match against pattern if given
            if let Some(pat) = pattern {
                let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
                let rel_str = rel.to_string_lossy();
                let name = entry.file_name().to_string_lossy();
                if !glob_match(pat, &name) && !glob_match(pat, &rel_str) {
                    continue;
                }
            }

            let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
            files.push(rel.to_string_lossy().into_owned());

            if files.len() >= args.max_results {
                truncated = true;
                break;
            }
        }

        files.sort();
        Ok(json!({ "files": files, "truncated": truncated }).to_string())
    }
}

fn glob_match(pattern: &str, name: &str) -> bool {
    let re_str = glob_to_regex(pattern);
    regex::Regex::new(&re_str)
        .map(|r| r.is_match(name))
        .unwrap_or(false)
}

fn glob_to_regex(pattern: &str) -> String {
    let mut result = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                result.push_str(".*");
            }
            '*' => result.push_str("[^/]*"),
            '?' => result.push_str("[^/]"),
            '.' | '+' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\' => {
                result.push('\\');
                result.push(c);
            }
            _ => result.push(c),
        }
    }
    result.push('$');
    result
}
