//! MCP client connections.
//!
//! Servers are reached over stdio (a spawned child process) or streamable HTTP
//! (a remote endpoint) — see [`crate::config::McpTransport`]. Unlike rig's
//! `McpClientHandler`, which registers MCP tools on the tool server directly,
//! the handler here wraps each one in a [`GatedTool`](crate::approval::GatedTool)
//! so remote tools hit the same approval gate and lifecycle hooks as the
//! built-in ones.

use std::collections::HashSet;
use std::sync::Arc;

use rig::tool::rmcp::McpTool;
use rig::tool::server::ToolServerHandle;
use rmcp::service::{NotificationContext, RoleClient, RunningService, ServerSink};
use rmcp::transport::child_process::TokioChildProcess;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{ClientHandler, ServiceExt};
use tokio::sync::RwLock;

use crate::approval::GateContext;
use crate::config::{McpServerConfig, McpTransport};
use crate::ui::AiEvent;

pub type McpService = RunningService<RoleClient, GatedMcpHandler>;

/// Client handler that registers an MCP server's tools behind the approval
/// gate, and re-registers them when the server announces a tool-list change.
pub struct GatedMcpHandler {
    client_info: rmcp::model::ClientInfo,
    handle: ToolServerHandle,
    ctx: GateContext,
    /// Server name from the config, used to attribute log/UI messages.
    server: String,
    /// Whether this server's tools require approval — true unless the server
    /// is marked `trusted`.
    dangerous: bool,
    /// Tool names this handler registered, so a refresh can remove exactly
    /// those and leave built-ins and other servers' tools alone.
    managed: Arc<RwLock<Vec<String>>>,
}

impl GatedMcpHandler {
    fn new(server: &McpServerConfig, handle: ToolServerHandle, ctx: GateContext) -> Self {
        Self {
            client_info: rmcp::model::ClientInfo::default(),
            handle,
            ctx,
            server: server.name.clone(),
            dangerous: !server.trusted,
            managed: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Surface a problem in the TUI — `tracing` output is invisible behind the
    /// alternate screen.
    fn warn(&self, msg: String) {
        tracing::warn!("{msg}");
        self.ctx.event_tx.send(AiEvent::Error(msg)).ok();
    }

    /// Tool names already registered on the shared tool server (built-ins plus
    /// any earlier MCP server's tools).
    async fn taken_names(&self) -> HashSet<String> {
        self.handle
            .get_tool_defs(None)
            .await
            .map(|defs| defs.into_iter().map(|d| d.name).collect())
            .unwrap_or_default()
    }

    /// Register `tools`, each wrapped in a `GatedTool`. A tool whose name is
    /// already taken is skipped rather than shadowing the existing one.
    async fn register(&self, tools: Vec<rmcp::model::Tool>, peer: &ServerSink) {
        let taken = self.taken_names().await;
        let mut managed = self.managed.write().await;
        for tool in tools {
            let name = tool.name.to_string();
            if taken.contains(&name) {
                self.warn(format!(
                    "MCP '{}': tool '{name}' skipped — that name is already registered",
                    self.server
                ));
                continue;
            }
            let gated = self
                .ctx
                .wrap(McpTool::from_mcp_server(tool, peer.clone()), self.dangerous);
            match self.handle.add_tool(gated).await {
                Ok(()) => managed.push(name),
                Err(e) => self.warn(format!(
                    "MCP '{}': registering tool '{name}' failed: {e}",
                    self.server
                )),
            }
        }
    }
}

impl ClientHandler for GatedMcpHandler {
    fn get_info(&self) -> rmcp::model::ClientInfo {
        self.client_info.clone()
    }

    async fn on_tool_list_changed(&self, context: NotificationContext<RoleClient>) {
        let tools = match context.peer.list_all_tools().await {
            Ok(tools) => tools,
            Err(e) => {
                self.warn(format!(
                    "MCP '{}': tool-list refresh failed: {e}",
                    self.server
                ));
                return;
            }
        };
        {
            let mut managed = self.managed.write().await;
            for name in managed.drain(..) {
                self.handle.remove_tool(&name).await.ok();
            }
        }
        self.register(tools, &context.peer).await;
        let count = self.managed.read().await.len();
        tracing::info!("MCP '{}': refreshed, {count} tools", self.server);
    }
}

/// Serve `handler` over `transport`, then register the server's initial tools.
async fn serve_and_register<T, E, A>(
    handler: GatedMcpHandler,
    transport: T,
) -> Result<McpService, String>
where
    T: rmcp::transport::IntoTransport<RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let service = ServiceExt::serve(handler, transport)
        .await
        .map_err(|e| format!("connect failed: {e}"))?;
    let tools = service
        .peer()
        .list_all_tools()
        .await
        .map_err(|e| format!("listing tools failed: {e}"))?;
    let count = tools.len();
    service.service().register(tools, service.peer()).await;
    tracing::info!("MCP connected, {count} tools offered");
    Ok(service)
}

fn http_transport(
    url: String,
    headers: &std::collections::HashMap<String, String>,
    bearer_token: Option<String>,
) -> Result<StreamableHttpClientTransport<reqwest::Client>, String> {
    let mut config = StreamableHttpClientTransportConfig::with_uri(url);
    if let Some(token) = bearer_token {
        config = config.auth_header(token);
    }
    if !headers.is_empty() {
        let parsed = headers
            .iter()
            .map(|(k, v)| {
                let name: http::HeaderName = k
                    .parse()
                    .map_err(|_| format!("invalid header name {k:?}"))?;
                let value: http::HeaderValue = v
                    .parse()
                    .map_err(|_| format!("invalid value for header {k:?}"))?;
                Ok((name, value))
            })
            .collect::<Result<_, String>>()?;
        config = config.custom_headers(parsed);
    }
    Ok(StreamableHttpClientTransport::from_config(config))
}

/// The tail of a child server's stderr, kept so a failed handshake can say why.
#[derive(Clone, Default)]
struct StderrLog(Arc<std::sync::Mutex<String>>);

impl StderrLog {
    /// Keep roughly this much of the tail — enough for a stack trace's last
    /// lines without letting a chatty server grow unboundedly.
    const CAP: usize = 4096;

    /// Read the child's stderr in the background. Draining matters even when
    /// nobody reads the result: a full pipe would block the server.
    fn drain(stderr: Option<tokio::process::ChildStderr>) -> Self {
        let log = Self::default();
        let Some(mut stderr) = stderr else {
            return log;
        };
        let sink = log.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buf = [0u8; 1024];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                sink.push(&String::from_utf8_lossy(&buf[..n]));
            }
        });
        log
    }

    fn push(&self, chunk: &str) {
        let mut buf = self.0.lock().unwrap();
        buf.push_str(chunk);
        if buf.len() > Self::CAP {
            // trim at a newline, which is always a char boundary
            let cut = buf.len() - Self::CAP / 2;
            if let Some(nl) = buf[cut..].find('\n') {
                *buf = buf[cut + nl + 1..].to_string();
            }
        }
    }

    /// Append whatever the server complained about to a connection error.
    async fn annotate(&self, err: String) -> String {
        // the reader task may still be catching up with the dying process
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let buf = self.0.lock().unwrap();
        let tail = buf.trim();
        if tail.is_empty() {
            err
        } else {
            format!("{err}\n    stderr: {}", tail.replace('\n', "\n    "))
        }
    }
}

/// Bound a connection attempt, so one unreachable server can't stall startup
/// for every other one.
async fn with_timeout<F>(limit: std::time::Duration, fut: F) -> Result<McpService, String>
where
    F: std::future::Future<Output = Result<McpService, String>>,
{
    match tokio::time::timeout(limit, fut).await {
        Ok(result) => result,
        Err(_) => Err(format!("timed out after {}s", limit.as_secs())),
    }
}

async fn connect_one(
    server: &McpServerConfig,
    handle: &ToolServerHandle,
    ctx: &GateContext,
) -> Result<McpService, String> {
    let transport = server.transport()?;
    let handler = GatedMcpHandler::new(server, handle.clone(), ctx.clone());
    let limit = server.timeout();

    match transport {
        McpTransport::Stdio { command, args, env } => {
            let mut cmd = tokio::process::Command::new(&command);
            cmd.args(&args);
            for (k, v) in &env {
                cmd.env(k, v);
            }
            // Pipe stderr rather than inheriting it: a server that logs to
            // stderr would otherwise scribble straight over the TUI.
            let (process, stderr) = TokioChildProcess::builder(cmd)
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| format!("spawning {command:?}: {e}"))?;
            let log = StderrLog::drain(stderr);
            match with_timeout(limit, serve_and_register(handler, process)).await {
                Ok(svc) => Ok(svc),
                Err(e) => Err(log.annotate(e).await),
            }
        }
        McpTransport::Http {
            url,
            headers,
            bearer_token,
        } => {
            let transport = http_transport(url, &headers, bearer_token)?;
            with_timeout(limit, serve_and_register(handler, transport)).await
        }
    }
}

/// Live status of one configured MCP server, as reported by `/mcp`.
pub struct McpStatus {
    pub name: String,
    /// Raw, unexpanded transport from the config, so a `${VAR}` standing in for
    /// a secret is never echoed back into the TUI.
    pub transport: String,
    pub trusted: bool,
    pub state: McpState,
}

pub enum McpState {
    /// Shares the handler's registered-tool list, so the report stays accurate
    /// for a server that changes its tools after connecting.
    Connected(Arc<RwLock<Vec<String>>>),
    Failed(String),
}

/// How a server is reached, described without expanding `${VAR}` references.
pub fn transport_label(server: &McpServerConfig) -> String {
    if let Some(url) = &server.url {
        format!("http {url}")
    } else if let Some(command) = &server.command {
        format!("stdio {command}")
    } else {
        "unconfigured".to_string()
    }
}

/// The `/mcp` report: every configured server, how it's reached, whether its
/// tools are gated, and what it currently offers.
pub async fn summary(statuses: &[McpStatus]) -> String {
    if statuses.is_empty() {
        return "No MCP servers configured.\n\nAdd [[mcp_servers]] entries to ~/.config/magai/config.toml\n(`command` for a local stdio server, `url` for a remote one),\nor install a plugin that bundles one.".to_string();
    }
    let mut lines = Vec::new();
    for status in statuses {
        let gating = if status.trusted {
            "trusted"
        } else {
            "approval required"
        };
        lines.push(format!(
            "● {} [{}] — {gating}",
            status.name, status.transport
        ));
        match &status.state {
            McpState::Connected(tools) => {
                let tools = tools.read().await;
                if tools.is_empty() {
                    lines.push("  connected, no tools registered".to_string());
                } else {
                    lines.push(format!(
                        "  connected, {} tool{}: {}",
                        tools.len(),
                        if tools.len() == 1 { "" } else { "s" },
                        tools.join(", ")
                    ));
                }
            }
            McpState::Failed(e) => lines.push(format!("  not connected: {e}")),
        }
    }
    lines.join("\n")
}

/// Connect to `servers` once and render their status — the `magai mcp list
/// --check` path, which runs with no agent or TUI behind it.
pub async fn check(servers: &[McpServerConfig]) -> String {
    use std::collections::HashMap;
    use std::sync::Mutex;

    let handle = rig::tool::server::ToolServer::new().run();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = GateContext {
        // nothing is called here, so the gate never comes into play
        mode: crate::approval::PermissionMode::Auto,
        gate: Arc::new(Mutex::new(HashMap::new())),
        event_tx,
        hook_runner: Arc::new(crate::hooks::HookRunner::new(Vec::new())),
        tool_outcomes: Arc::new(Mutex::new((0, 0, 0))),
        smart: None,
        safe_tools: Default::default(),
    };

    let (services, statuses) = connect_servers(servers, handle, ctx).await;
    let report = summary(&statuses).await;
    // failures are already in `statuses`; drain so nothing is left dangling
    drop(services);
    event_rx.close();
    report
}

/// Connect every configured server, returning the running services (which must
/// be held to keep the connections alive) alongside their status.
pub async fn connect_servers(
    servers: &[McpServerConfig],
    handle: ToolServerHandle,
    ctx: GateContext,
) -> (Vec<McpService>, Vec<McpStatus>) {
    let mut services = Vec::new();
    let mut statuses = Vec::new();

    for server in servers {
        let state = match connect_one(server, &handle, &ctx).await {
            Ok(svc) => {
                tracing::info!("MCP '{}' connected", server.name);
                let tools = svc.service().managed.clone();
                services.push(svc);
                McpState::Connected(tools)
            }
            Err(e) => {
                let msg = format!("MCP '{}': {e}", server.name);
                tracing::error!("{msg}");
                ctx.event_tx.send(AiEvent::Error(msg)).ok();
                McpState::Failed(e)
            }
        };
        statuses.push(McpStatus {
            name: server.name.clone(),
            transport: transport_label(server),
            trusted: server.trusted,
            state,
        });
    }

    (services, statuses)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use rig::tool::server::ToolServer;
    use rmcp::model::*;
    use rmcp::service::RequestContext;
    use rmcp::{RoleServer, ServerHandler};
    use tokio::sync::mpsc;

    use super::*;
    use crate::approval::{ApprovalGate, PermissionMode, ToolOutcomeCounter};
    use crate::hooks::HookRunner;

    /// A minimal in-process MCP server exposing a single `echo` tool.
    #[derive(Clone)]
    struct EchoServer;

    impl ServerHandler for EchoServer {
        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
                .with_protocol_version(ProtocolVersion::LATEST)
                .with_server_info(Implementation::new("echo-server", "0.1.0"))
        }

        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, ErrorData> {
            Ok(ListToolsResult::with_all_items(vec![Tool::new(
                "echo".to_string(),
                "echoes its input".to_string(),
                Arc::new(serde_json::Map::new()),
            )]))
        }

        async fn call_tool(
            &self,
            _request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<CallToolResult, ErrorData> {
            Ok(CallToolResult::success(vec![Content::text("echoed")]))
        }
    }

    fn gate_context(
        mode: PermissionMode,
    ) -> (GateContext, mpsc::UnboundedReceiver<AiEvent>, ApprovalGate) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let gate: ApprovalGate = Arc::new(Mutex::new(HashMap::new()));
        let outcomes: ToolOutcomeCounter = Arc::new(Mutex::new((0, 0, 0)));
        (
            GateContext {
                mode,
                gate: gate.clone(),
                event_tx,
                hook_runner: Arc::new(HookRunner::new(Vec::new())),
                tool_outcomes: outcomes,
                smart: None,
                safe_tools: Default::default(),
            },
            event_rx,
            gate,
        )
    }

    /// Connect an `EchoServer` over an in-memory duplex pair and register its
    /// tools through `handler`, exactly as a real connection would.
    async fn connect_echo(handler: GatedMcpHandler) -> McpService {
        let (client_to_server, server_from_client) = tokio::io::duplex(8192);
        let (server_to_client, client_from_server) = tokio::io::duplex(8192);

        tokio::spawn(async move {
            let service = EchoServer
                .serve((server_from_client, server_to_client))
                .await
                .expect("server start");
            service.waiting().await.ok();
        });

        serve_and_register(handler, (client_from_server, client_to_server))
            .await
            .expect("connect")
    }

    fn config(name: &str, trusted: bool) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            trusted,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn mcp_tool_call_goes_through_the_approval_gate() {
        let handle = ToolServer::new().run();
        let (ctx, mut events, gate) = gate_context(PermissionMode::AskDangerous);
        let _svc = connect_echo(GatedMcpHandler::new(
            &config("echo-srv", false),
            handle.clone(),
            ctx,
        ))
        .await;

        let calling = tokio::spawn({
            let handle = handle.clone();
            async move { handle.call_tool("echo", "{}").await }
        });

        // The call must block on an approval request rather than running.
        let call_id = loop {
            match events.recv().await.expect("approval event") {
                AiEvent::ToolCallApprovalRequired {
                    call_id,
                    name,
                    is_dangerous,
                    ..
                } => {
                    assert_eq!(name, "echo");
                    assert!(is_dangerous, "untrusted MCP tools are dangerous");
                    break call_id;
                }
                _ => continue,
            }
        };

        let approve = gate.lock().unwrap().remove(&call_id).expect("gate entry");
        approve.send(true).ok();
        assert_eq!(calling.await.unwrap().unwrap(), "echoed");
    }

    #[tokio::test]
    async fn trusted_server_tools_skip_the_gate() {
        let handle = ToolServer::new().run();
        let (ctx, _events, _gate) = gate_context(PermissionMode::AskDangerous);
        let _svc = connect_echo(GatedMcpHandler::new(
            &config("echo-srv", true),
            handle.clone(),
            ctx,
        ))
        .await;

        assert_eq!(handle.call_tool("echo", "{}").await.unwrap(), "echoed");
    }

    #[tokio::test]
    async fn summary_reports_transport_gating_and_tools() {
        let statuses = vec![
            McpStatus {
                name: "docs".to_string(),
                transport: "http https://mcp.example.com/mcp".to_string(),
                trusted: false,
                state: McpState::Connected(Arc::new(RwLock::new(vec![
                    "search".to_string(),
                    "fetch".to_string(),
                ]))),
            },
            McpStatus {
                name: "fs".to_string(),
                transport: "stdio npx".to_string(),
                trusted: true,
                state: McpState::Failed("spawning \"npx\": not found".to_string()),
            },
        ];

        let report = summary(&statuses).await;
        assert!(report.contains("docs [http https://mcp.example.com/mcp]"));
        assert!(report.contains("approval required"));
        assert!(report.contains("connected, 2 tools: search, fetch"));
        assert!(report.contains("fs [stdio npx]"));
        assert!(report.contains("trusted"));
        assert!(report.contains("not connected: spawning \"npx\": not found"));
    }

    #[tokio::test]
    async fn summary_without_servers_explains_how_to_add_one() {
        let report = summary(&[]).await;
        assert!(report.contains("No MCP servers configured"));
        assert!(report.contains("[[mcp_servers]]"));
    }

    #[tokio::test]
    async fn unreachable_server_is_reported_not_silently_dropped() {
        let handle = ToolServer::new().run();
        let (ctx, mut events, _gate) = gate_context(PermissionMode::Auto);
        // neither `command` nor `url`: rejected before any process is spawned
        let (services, statuses) = connect_servers(&[config("broken", false)], handle, ctx).await;

        assert!(services.is_empty());
        assert!(matches!(statuses[0].state, McpState::Failed(_)));
        assert_eq!(statuses[0].transport, "unconfigured");
        assert!(matches!(
            events.try_recv(),
            Ok(AiEvent::Error(msg)) if msg.contains("broken")
        ));
        assert!(summary(&statuses).await.contains("not connected"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_that_never_answers_times_out() {
        let handle = ToolServer::new().run();
        let (ctx, _events, _gate) = gate_context(PermissionMode::Auto);
        let mut server = config("silent", false);
        // a pipe nobody ever writes to: the handshake can never complete
        server.command = Some("sleep".to_string());
        server.args = vec!["600".to_string()];
        server.timeout_secs = Some(5);

        let (services, statuses) = connect_servers(&[server], handle, ctx).await;

        assert!(services.is_empty());
        match &statuses[0].state {
            McpState::Failed(e) => assert!(e.contains("timed out after 5s"), "{e}"),
            _ => panic!("expected a timeout failure"),
        }
    }

    #[tokio::test]
    async fn colliding_tool_name_is_skipped_not_shadowed() {
        let handle = ToolServer::new().run();
        let (ctx, _events, _gate) = gate_context(PermissionMode::Auto);

        let _first = connect_echo(GatedMcpHandler::new(
            &config("first", true),
            handle.clone(),
            ctx.clone(),
        ))
        .await;
        let _second = connect_echo(GatedMcpHandler::new(
            &config("second", true),
            handle.clone(),
            ctx,
        ))
        .await;

        let defs = handle.get_tool_defs(None).await.unwrap();
        let echoes = defs.iter().filter(|d| d.name == "echo").count();
        assert_eq!(echoes, 1, "second server's colliding tool must be skipped");
    }
}
