//! Command-line interface.
//!
//! With no subcommand `magai` launches the TUI; the subcommands here manage
//! `~/.config/magai/config.toml` from the shell. Edits go through `toml_edit`
//! so the rest of the file — comments included — survives untouched.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use toml_edit::{value, Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table};

use crate::config::{Config, McpServerConfig};

#[derive(Parser)]
#[command(name = "magai", version, about = "A terminal coding agent")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Manage MCP servers
    #[command(subcommand)]
    Mcp(McpCommand),
}

#[derive(Subcommand)]
pub enum McpCommand {
    /// Add an MCP server to the config
    Add(AddArgs),
    /// List configured MCP servers
    List {
        /// Connect to each server and report what it offers
        #[arg(long)]
        check: bool,
    },
    /// Show one server's full configuration
    Get { name: String },
    /// Remove a server from the config
    #[command(alias = "rm")]
    Remove { name: String },
}

#[derive(Copy, Clone, PartialEq, ValueEnum)]
pub enum Transport {
    /// Spawn a local command and talk over its stdin/stdout
    Stdio,
    /// Connect to a remote streamable-HTTP endpoint
    Http,
}

#[derive(Args)]
pub struct AddArgs {
    /// Name used to refer to this server
    name: String,
    /// Command to spawn, or the endpoint URL when --transport http
    command_or_url: String,
    /// Arguments for the command; pass them after `--` if they start with `-`
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
    #[arg(short = 't', long, value_enum, default_value = "stdio")]
    transport: Transport,
    /// Environment variable for the spawned command (repeatable)
    #[arg(short = 'e', long = "env", value_name = "KEY=VALUE")]
    env: Vec<String>,
    /// HTTP header for a remote server (repeatable)
    #[arg(short = 'H', long = "header", value_name = "NAME:VALUE")]
    header: Vec<String>,
    /// Token sent as `Authorization: Bearer <token>`
    #[arg(long, value_name = "TOKEN")]
    bearer_token: Option<String>,
    /// Exempt this server's tools from the approval prompt
    #[arg(long)]
    trusted: bool,
    /// Overwrite an existing server with the same name
    #[arg(long)]
    force: bool,
}

/// Splits `KEY=VALUE` on the first `=`; the value may contain further `=`.
fn parse_env(raw: &str) -> Result<(String, String), String> {
    let (key, val) = raw
        .split_once('=')
        .ok_or_else(|| format!("expected KEY=VALUE, got {raw:?}"))?;
    if key.trim().is_empty() {
        return Err(format!("empty variable name in {raw:?}"));
    }
    Ok((key.trim().to_string(), val.to_string()))
}

/// Splits `Name: Value` on the first `:`; the value may contain further `:`
/// (a URL, say), and surrounding whitespace is dropped.
fn parse_header(raw: &str) -> Result<(String, String), String> {
    let (name, val) = raw
        .split_once(':')
        .ok_or_else(|| format!("expected NAME:VALUE, got {raw:?}"))?;
    if name.trim().is_empty() {
        return Err(format!("empty header name in {raw:?}"));
    }
    Ok((name.trim().to_string(), val.trim().to_string()))
}

fn pairs(
    raw: &[String],
    parse: fn(&str) -> Result<(String, String), String>,
) -> Result<InlineTable, String> {
    let mut table = InlineTable::new();
    for item in raw {
        let (k, v) = parse(item)?;
        table.insert(&k, v.into());
    }
    Ok(table)
}

fn index_of(servers: &ArrayOfTables, name: &str) -> Option<usize> {
    servers
        .iter()
        .position(|t| t.get("name").and_then(|n| n.as_str()) == Some(name))
}

/// The `[[mcp_servers]]` array, created if the document doesn't have one yet.
fn servers_mut(doc: &mut DocumentMut) -> Result<&mut ArrayOfTables, String> {
    doc.entry("mcp_servers")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()))
        .as_array_of_tables_mut()
        .ok_or_else(|| "`mcp_servers` in the config is not a list of servers".to_string())
}

/// Append (or, with `force`, replace) one `[[mcp_servers]]` entry.
fn add_server(doc: &mut DocumentMut, args: &AddArgs, force: bool) -> Result<(), String> {
    let mut table = Table::new();
    table["name"] = value(&args.name);

    match args.transport {
        Transport::Stdio => {
            if !args.header.is_empty() || args.bearer_token.is_some() {
                return Err("--header and --bearer-token apply to --transport http".to_string());
            }
            table["command"] = value(&args.command_or_url);
            if !args.args.is_empty() {
                let mut arr = Array::new();
                for a in &args.args {
                    arr.push(a.as_str());
                }
                table["args"] = value(arr);
            }
            if !args.env.is_empty() {
                table["env"] = value(pairs(&args.env, parse_env)?);
            }
        }
        Transport::Http => {
            if !args.env.is_empty() {
                return Err("--env applies to --transport stdio".to_string());
            }
            if !args.args.is_empty() {
                return Err(format!(
                    "a remote server takes no arguments (got {:?})",
                    args.args
                ));
            }
            if !args.command_or_url.starts_with("http://")
                && !args.command_or_url.starts_with("https://")
            {
                return Err(format!(
                    "--transport http needs a URL, got {:?}",
                    args.command_or_url
                ));
            }
            table["url"] = value(&args.command_or_url);
            if let Some(token) = &args.bearer_token {
                table["bearer_token"] = value(token);
            }
            if !args.header.is_empty() {
                table["headers"] = value(pairs(&args.header, parse_header)?);
            }
        }
    }

    if args.trusted {
        table["trusted"] = value(true);
    }

    let servers = servers_mut(doc)?;
    match index_of(servers, &args.name) {
        // replaced in place, so the config file keeps its order
        Some(idx) if force => *servers.get_mut(idx).expect("index just found") = table,
        Some(_) => {
            return Err(format!(
                "a server named {:?} already exists — pass --force to replace it",
                args.name
            ))
        }
        None => servers.push(table),
    }
    Ok(())
}

/// Drop the named entry. `Ok(false)` means there was nothing to remove.
fn remove_server(doc: &mut DocumentMut, name: &str) -> Result<bool, String> {
    let Some(servers) = doc
        .get_mut("mcp_servers")
        .and_then(|i| i.as_array_of_tables_mut())
    else {
        return Ok(false);
    };
    match index_of(servers, name) {
        Some(idx) => {
            servers.remove(idx);
            if servers.is_empty() {
                doc.remove("mcp_servers");
            }
            Ok(true)
        }
        None => Ok(false),
    }
}

fn config_file() -> Result<PathBuf, String> {
    crate::config::config_path()
        .ok_or_else(|| "could not locate a config directory (is $HOME set?)".to_string())
}

fn read_document(path: &PathBuf) -> Result<DocumentMut, String> {
    if !path.exists() {
        return Ok(DocumentMut::new());
    }
    std::fs::read_to_string(path)
        .map_err(|e| format!("reading {}: {e}", path.display()))?
        .parse::<DocumentMut>()
        .map_err(|e| format!("{} is not valid TOML: {e}", path.display()))
}

fn write_document(path: &PathBuf, doc: &DocumentMut) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    std::fs::write(path, doc.to_string()).map_err(|e| format!("writing {}: {e}", path.display()))
}

/// Every server magai would start: those in the config plus any a plugin
/// bundles. Plugin entries are read-only here — they live in the plugin.
fn all_servers() -> (Vec<McpServerConfig>, Vec<McpServerConfig>) {
    let (cfg, warning) = Config::load();
    if let Some(warning) = warning {
        eprintln!("warning: {warning}");
    }
    let plugins = crate::plugins::extract_mcp_configs(&crate::plugins::discover());
    (cfg.mcp_servers, plugins)
}

fn describe(server: &McpServerConfig) -> String {
    let mut label = crate::mcp::transport_label(server);
    if !server.args.is_empty() {
        label.push(' ');
        label.push_str(&server.args.join(" "));
    }
    let gating = if server.trusted {
        "trusted"
    } else {
        "approval required"
    };
    format!("{label} — {gating}")
}

async fn list(check: bool) -> Result<(), String> {
    let (configured, from_plugins) = all_servers();
    if configured.is_empty() && from_plugins.is_empty() {
        println!("No MCP servers configured.");
        println!("\nAdd one with:");
        println!("  magai mcp add <name> <command> [args...]");
        println!("  magai mcp add --transport http <name> <url>");
        return Ok(());
    }

    if check {
        let all: Vec<_> = configured
            .iter()
            .chain(from_plugins.iter())
            .cloned()
            .collect();
        println!("{}", crate::mcp::check(&all).await);
        return Ok(());
    }

    for server in &configured {
        println!("{:<20} {}", server.name, describe(server));
    }
    for server in &from_plugins {
        println!("{:<20} {} [from plugin]", server.name, describe(server));
    }
    println!("\nRun `magai mcp list --check` to connect and see each server's tools.");
    Ok(())
}

fn get(name: &str) -> Result<(), String> {
    let (configured, from_plugins) = all_servers();
    let Some(server) = configured
        .iter()
        .chain(from_plugins.iter())
        .find(|s| s.name == name)
    else {
        return Err(format!("no MCP server named {name:?}"));
    };

    println!("{}", server.name);
    if let Some(url) = &server.url {
        println!("  transport    http");
        println!("  url          {url}");
        if server.bearer_token.is_some() {
            println!("  bearer_token (set)");
        }
        for (k, v) in &server.headers {
            println!("  header       {k}: {v}");
        }
    } else if let Some(command) = &server.command {
        println!("  transport    stdio");
        println!("  command      {command}");
        if !server.args.is_empty() {
            println!("  args         {}", server.args.join(" "));
        }
        for (k, v) in &server.env {
            println!("  env          {k}={v}");
        }
    } else {
        println!("  transport    none — needs a `command` or a `url`");
    }
    println!(
        "  tools        {}",
        if server.trusted {
            "trusted (no approval prompt)"
        } else {
            "approval required"
        }
    );
    Ok(())
}

fn add(args: &AddArgs) -> Result<(), String> {
    let path = config_file()?;
    let mut doc = read_document(&path)?;
    add_server(&mut doc, args, args.force)?;
    write_document(&path, &doc)?;
    println!("Added MCP server {:?} to {}", args.name, path.display());
    if !args.trusted {
        println!("Its tools will ask for approval before each call (--trusted to skip that).");
    }
    Ok(())
}

fn remove(name: &str) -> Result<(), String> {
    let path = config_file()?;
    let mut doc = read_document(&path)?;
    if !remove_server(&mut doc, name)? {
        return Err(format!(
            "no MCP server named {name:?} in {}",
            path.display()
        ));
    }
    write_document(&path, &doc)?;
    println!("Removed MCP server {name:?} from {}", path.display());
    Ok(())
}

/// Run a subcommand. Returns an error message suitable for stderr.
pub async fn run(command: Command) -> Result<(), String> {
    match command {
        Command::Mcp(McpCommand::Add(args)) => add(&args),
        Command::Mcp(McpCommand::List { check }) => list(check).await,
        Command::Mcp(McpCommand::Get { name }) => get(&name),
        Command::Mcp(McpCommand::Remove { name }) => remove(&name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add_args(name: &str, command_or_url: &str, transport: Transport) -> AddArgs {
        AddArgs {
            name: name.to_string(),
            command_or_url: command_or_url.to_string(),
            args: Vec::new(),
            transport,
            env: Vec::new(),
            header: Vec::new(),
            bearer_token: None,
            trusted: false,
            force: false,
        }
    }

    #[test]
    fn parses_env_and_header_values_containing_separators() {
        assert_eq!(
            parse_env("TOKEN=a=b").unwrap(),
            ("TOKEN".to_string(), "a=b".to_string())
        );
        assert_eq!(
            parse_header("X-Origin: https://example.com").unwrap(),
            ("X-Origin".to_string(), "https://example.com".to_string())
        );
        assert!(parse_env("TOKEN").is_err());
        assert!(parse_header("nope").is_err());
        assert!(parse_env("=orphan").is_err());
    }

    #[test]
    fn add_stdio_server_keeps_surrounding_config() {
        let mut doc = "# my config\ndefault_model = \"local\"\n"
            .parse::<DocumentMut>()
            .unwrap();
        let mut args = add_args("fs", "npx", Transport::Stdio);
        args.args = vec!["-y".to_string(), "server-filesystem".to_string()];
        args.env = vec!["TOKEN=abc".to_string()];
        add_server(&mut doc, &args, false).unwrap();

        let out = doc.to_string();
        assert!(out.contains("# my config"), "comments must survive: {out}");
        assert!(out.contains("default_model = \"local\""));

        let cfg: Config = toml::from_str(&out).expect("still valid config");
        let server = &cfg.mcp_servers[0];
        assert_eq!(server.command.as_deref(), Some("npx"));
        assert_eq!(server.args, vec!["-y", "server-filesystem"]);
        assert_eq!(server.env.get("TOKEN").map(String::as_str), Some("abc"));
        assert!(!server.trusted);
    }

    #[test]
    fn add_http_server_records_url_headers_and_token() {
        let mut doc = DocumentMut::new();
        let mut args = add_args("docs", "https://mcp.example.com/mcp", Transport::Http);
        args.header = vec!["X-Tenant: acme".to_string()];
        args.bearer_token = Some("${DOCS_TOKEN}".to_string());
        args.trusted = true;
        add_server(&mut doc, &args, false).unwrap();

        let cfg: Config = toml::from_str(&doc.to_string()).expect("valid config");
        let server = &cfg.mcp_servers[0];
        assert_eq!(server.url.as_deref(), Some("https://mcp.example.com/mcp"));
        assert_eq!(server.bearer_token.as_deref(), Some("${DOCS_TOKEN}"));
        assert_eq!(
            server.headers.get("X-Tenant").map(String::as_str),
            Some("acme")
        );
        assert!(server.trusted);
    }

    #[test]
    fn add_rejects_flags_that_do_not_match_the_transport() {
        let mut doc = DocumentMut::new();
        let mut stdio = add_args("fs", "npx", Transport::Stdio);
        stdio.bearer_token = Some("tok".to_string());
        assert!(add_server(&mut doc, &stdio, false).is_err());

        let mut http = add_args("docs", "https://example.com/mcp", Transport::Http);
        http.env = vec!["A=b".to_string()];
        assert!(add_server(&mut doc, &http, false).is_err());

        let not_a_url = add_args("docs", "npx", Transport::Http);
        assert!(add_server(&mut doc, &not_a_url, false).is_err());

        assert!(doc.get("mcp_servers").is_none(), "nothing partial written");
    }

    #[test]
    fn add_refuses_duplicate_name_unless_forced() {
        let mut doc = DocumentMut::new();
        add_server(&mut doc, &add_args("fs", "npx", Transport::Stdio), false).unwrap();

        let err =
            add_server(&mut doc, &add_args("fs", "uvx", Transport::Stdio), false).unwrap_err();
        assert!(err.contains("--force"), "{err}");

        add_server(&mut doc, &add_args("fs", "uvx", Transport::Stdio), true).unwrap();
        let cfg: Config = toml::from_str(&doc.to_string()).unwrap();
        assert_eq!(cfg.mcp_servers.len(), 1);
        assert_eq!(cfg.mcp_servers[0].command.as_deref(), Some("uvx"));
    }

    #[test]
    fn remove_drops_only_the_named_server() {
        let mut doc = DocumentMut::new();
        add_server(&mut doc, &add_args("a", "cmd-a", Transport::Stdio), false).unwrap();
        add_server(&mut doc, &add_args("b", "cmd-b", Transport::Stdio), false).unwrap();

        assert!(remove_server(&mut doc, "a").unwrap());
        assert!(!remove_server(&mut doc, "a").unwrap(), "already gone");

        let cfg: Config = toml::from_str(&doc.to_string()).unwrap();
        assert_eq!(cfg.mcp_servers.len(), 1);
        assert_eq!(cfg.mcp_servers[0].name, "b");

        assert!(remove_server(&mut doc, "b").unwrap());
        assert!(
            doc.get("mcp_servers").is_none(),
            "an empty array is cleaned up"
        );
    }

    #[test]
    fn remove_from_a_config_without_servers_is_not_an_error() {
        let mut doc = "default_model = \"local\"\n"
            .parse::<DocumentMut>()
            .unwrap();
        assert!(!remove_server(&mut doc, "nope").unwrap());
    }
}
