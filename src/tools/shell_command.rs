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

/// Whether a `shell_command` call needs approval under `ask-dangerous`/`smart`.
/// Fails closed: only commands on a read-only allowlist (or matching one of the
/// user's `[permissions] safe_commands` prefixes) are safe; anything unknown or
/// unparsable is dangerous. No shell is involved (`Command::new` + args), so
/// pipes and redirections cannot smuggle a write past this check — but a shell
/// or interpreter as the command itself can, hence those are never safe.
pub fn is_dangerous(args_json: &str, user_safe: &[Vec<String>]) -> bool {
    let Ok(args) = serde_json::from_str::<ShellCmdArgs>(args_json) else {
        return true;
    };
    let program = args
        .command
        .rsplit('/')
        .next()
        .unwrap_or(&args.command)
        .to_string();
    if user_safe
        .iter()
        .any(|prefix| matches_prefix(&args.command, &program, &args.args, prefix))
    {
        return false;
    }
    !builtin_safe(&program, &args.args)
}

/// `prefix` is `[command, arg...]`; the command matches by exact path or by
/// program name, and the call's args must start with the remaining tokens.
fn matches_prefix(command: &str, program: &str, args: &[String], prefix: &[String]) -> bool {
    let Some((head, rest)) = prefix.split_first() else {
        return false;
    };
    (head == command || head == program)
        && args.len() >= rest.len()
        && args.iter().zip(rest).all(|(a, p)| a == p)
}

fn builtin_safe(program: &str, args: &[String]) -> bool {
    let has = |flags: &[&str]| {
        args.iter().any(|a| {
            flags
                .iter()
                .any(|f| a == f || a.starts_with(&format!("{f}=")))
        })
    };
    match program {
        "ls" | "pwd" | "cat" | "head" | "tail" | "wc" | "echo" | "which" | "whoami" | "file"
        | "stat" | "du" | "df" | "grep" | "diff" | "uname" | "printenv" => true,
        // `-o` writes the listing to a file.
        "tree" => !has(&["-o"]),
        // `--pre` runs an arbitrary preprocessor command on every file.
        "rg" => !has(&["--pre"]),
        "find" => !args.iter().any(|a| {
            matches!(
                a.as_str(),
                "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir"
            ) || a.starts_with("-fprint")
                || a.starts_with("-fls")
        }),
        "git" => git_safe(args),
        "cargo" => cargo_safe(args),
        _ => false,
    }
}

fn git_safe(args: &[String]) -> bool {
    // Global options (`-c`, `--git-dir`, `--exec-path`, …) can change what a
    // read-only subcommand does, so only `--no-pager` may precede it.
    let args: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .skip_while(|a| *a == "--no-pager")
        .collect();
    let Some((sub, rest)) = args.split_first() else {
        return false;
    };
    if rest.iter().any(|a| a.starts_with("--output")) {
        return false;
    }
    let only = |allowed: &[&str]| rest.iter().all(|a| allowed.contains(a));
    match *sub {
        "status" | "log" | "diff" | "show" | "rev-parse" | "ls-files" | "blame" | "shortlog"
        | "describe" => true,
        // Any positional argument would create a branch; delete/move/copy are flags.
        "branch" => only(&[
            "-a",
            "-r",
            "-v",
            "-vv",
            "--all",
            "--remotes",
            "--verbose",
            "--list",
            "--show-current",
        ]),
        "remote" => only(&["-v", "--verbose"]),
        "tag" => only(&["-l", "--list"]),
        _ => false,
    }
}

fn cargo_safe(args: &[String]) -> bool {
    let Some((sub, rest)) = args.split_first() else {
        return false;
    };
    match sub.as_str() {
        "check" | "build" | "test" | "doc" | "tree" | "metadata" | "--version" => true,
        "clippy" => !rest.iter().any(|a| a == "--fix"),
        "fmt" => rest.iter().any(|a| a == "--check"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(command: &str, args: &[&str]) -> String {
        json!({ "command": command, "args": args }).to_string()
    }

    #[test]
    fn read_only_commands_are_safe() {
        for (cmd, args) in [
            ("ls", &["-la"][..]),
            ("/bin/ls", &[][..]),
            ("git", &["status"][..]),
            ("git", &["--no-pager", "log", "-5"][..]),
            ("git", &["branch", "-a"][..]),
            ("cargo", &["test"][..]),
            ("cargo", &["fmt", "--check"][..]),
            ("find", &[".", "-name", "*.rs"][..]),
            ("rg", &["foo"][..]),
        ] {
            assert!(!is_dangerous(&call(cmd, args), &[]), "{cmd} {args:?}");
        }
    }

    #[test]
    fn writes_and_unknowns_are_dangerous() {
        for (cmd, args) in [
            ("rm", &["-rf", "target"][..]),
            ("git", &["push"][..]),
            ("git", &["-c", "core.pager=sh", "log"][..]),
            ("git", &["branch", "new-branch"][..]),
            ("git", &["diff", "--output=x"][..]),
            ("find", &[".", "-delete"][..]),
            ("bash", &["-c", "ls"][..]),
            ("cargo", &["run"][..]),
            ("cargo", &["fmt"][..]),
            ("cargo", &["clippy", "--fix"][..]),
            ("rg", &["--pre", "sh", "x"][..]),
            ("env", &["rm", "x"][..]),
            ("git", &[][..]),
        ] {
            assert!(is_dangerous(&call(cmd, args), &[]), "{cmd} {args:?}");
        }
        assert!(is_dangerous("not json", &[]));
    }

    #[test]
    fn user_prefixes_mark_commands_safe() {
        let safe = vec![
            vec!["make".to_string(), "test".to_string()],
            vec!["npm".to_string()],
        ];
        assert!(!is_dangerous(&call("make", &["test", "-j4"]), &safe));
        assert!(!is_dangerous(&call("/usr/bin/npm", &["install"]), &safe));
        assert!(is_dangerous(&call("make", &["install"]), &safe));
        assert!(is_dangerous(&call("make", &[]), &safe));
    }
}
