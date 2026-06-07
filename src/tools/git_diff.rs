use rig::completion::request::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::process::Command;

const MAX_DIFF_BYTES: usize = 20_000;

#[derive(Deserialize)]
pub struct GitDiffArgs {
    pub path: Option<String>,
    #[serde(default)]
    pub staged: bool,
    pub file: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub struct GitDiff;

impl Tool for GitDiff {
    const NAME: &'static str = "git_diff";
    type Error = std::io::Error;
    type Args = GitDiffArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "git_diff".to_string(),
            description: "Show a git diff. Use staged=true for the index (staged changes), omit for working tree changes. Optionally scope to a single file.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "path to the git repository (defaults to current directory)" },
                    "staged": { "type": "boolean", "description": "diff staged (index) changes instead of working tree (default false)" },
                    "file": { "type": "string", "description": "limit diff to this file path" }
                }
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let mut cmd = Command::new("git");
        cmd.arg("diff");
        if args.staged {
            cmd.arg("--staged");
        }
        if let Some(ref f) = args.file {
            cmd.arg("--").arg(f);
        }
        if let Some(ref p) = args.path {
            cmd.current_dir(p);
        }

        let output = cmd.output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Ok(json!({ "error": stderr.trim() }).to_string());
        }

        let full = String::from_utf8_lossy(&output.stdout);
        let truncated = full.len() > MAX_DIFF_BYTES;
        let diff = if truncated {
            full[..MAX_DIFF_BYTES].to_string()
        } else {
            full.into_owned()
        };

        Ok(json!({ "diff": diff, "truncated": truncated }).to_string())
    }
}
