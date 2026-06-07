use rig::completion::request::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::process::Command;

#[derive(Deserialize)]
pub struct GitStatusArgs {
    pub path: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub struct GitStatus;

impl Tool for GitStatus {
    const NAME: &'static str = "git_status";
    type Error = std::io::Error;
    type Args = GitStatusArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "git_status".to_string(),
            description: "Show the current git status: branch name and staged, unstaged, and untracked files.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "path to the git repository (defaults to current directory)" }
                }
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let mut cmd = Command::new("git");
        cmd.args(["status", "--porcelain", "-b"]);
        if let Some(ref p) = args.path {
            cmd.current_dir(p);
        }

        let output = cmd.output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Ok(json!({ "error": stderr.trim() }).to_string());
        }

        let text = String::from_utf8_lossy(&output.stdout);
        let (branch, staged, unstaged, untracked) = parse_porcelain(&text);

        Ok(json!({
            "branch": branch,
            "staged": staged,
            "unstaged": unstaged,
            "untracked": untracked
        })
        .to_string())
    }
}

fn parse_porcelain(output: &str) -> (String, Vec<String>, Vec<String>, Vec<String>) {
    let mut branch = String::from("unknown");
    let mut staged = Vec::new();
    let mut unstaged = Vec::new();
    let mut untracked = Vec::new();

    for line in output.lines() {
        if let Some(info) = line.strip_prefix("## ") {
            // "main...origin/main [ahead 1]" or just "main" or "HEAD (no branch)"
            branch = info.split("...").next().unwrap_or(info).to_string();
        } else if line.len() >= 3 {
            let mut chars = line.chars();
            let x = chars.next().unwrap_or(' ');
            let y = chars.next().unwrap_or(' ');
            let path = line[3..].to_string();

            if x == '?' && y == '?' {
                untracked.push(path);
            } else {
                if x != ' ' {
                    staged.push(format!("{} {}", x, path));
                }
                if y != ' ' {
                    unstaged.push(format!("{} {}", y, path));
                }
            }
        }
    }

    (branch, staged, unstaged, untracked)
}
