use rig::completion::request::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::process::Stdio;
use tokio::process::Command;
use tokio::time::timeout;

fn default_timeout() -> u64 {
    30
}

#[derive(Deserialize)]
pub struct ShellCmdArgs {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

#[derive(Deserialize, Serialize)]
pub struct ShellCmd;

impl Tool for ShellCmd {
    const NAME: &'static str = "shell_command";
    type Error = std::io::Error;
    type Args = ShellCmdArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "shell_command".to_string(),
            description: "Run a shell command. Prefer dedicated tools (read_file, write_file, grep_search, find_files, git_status, git_diff) for file and git operations; use this for build/test/install commands and anything else.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "command name to run" },
                    "args": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "arguments to pass to the command"
                    },
                    "cwd": { "type": "string", "description": "working directory (defaults to current directory)" },
                    "timeout_secs": { "type": "integer", "description": "max seconds to wait before killing the process (default 30, max 300)" }
                },
                "required": ["command"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let timeout_secs = args.timeout_secs.min(300);

        let mut cmd = Command::new(&args.command);
        cmd.args(&args.args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if let Some(ref cwd) = args.cwd {
            cmd.current_dir(cwd);
        }

        let child = cmd.spawn()?;

        match timeout(
            std::time::Duration::from_secs(timeout_secs),
            child.wait_with_output(),
        )
        .await
        {
            Ok(Ok(output)) => Ok(json!({
                "stdout": String::from_utf8_lossy(&output.stdout),
                "stderr": String::from_utf8_lossy(&output.stderr),
                "exit_code": output.status.code(),
                "timed_out": false
            })
            .to_string()),
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(json!({
                "stdout": "",
                "stderr": format!("command timed out after {} seconds", timeout_secs),
                "exit_code": null,
                "timed_out": true
            })
            .to_string()),
        }
    }
}
