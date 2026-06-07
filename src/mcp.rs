use rig::tool::rmcp::McpClientHandler;
use rig::tool::server::ToolServerHandle;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::child_process::TokioChildProcess;

use crate::config::McpServerConfig;

pub type McpService = RunningService<RoleClient, McpClientHandler>;

pub async fn connect_servers(
    servers: &[McpServerConfig],
    handle: ToolServerHandle,
) -> Vec<McpService> {
    let mut services = Vec::new();

    for server in servers {
        let client_info = rmcp::model::ClientInfo::default();

        let mut cmd = tokio::process::Command::new(&server.command);
        cmd.args(&server.args);
        for (k, v) in &server.env {
            cmd.env(k, v);
        }

        let process = match TokioChildProcess::new(cmd) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("MCP '{}' spawn failed: {e}", server.name);
                continue;
            }
        };

        let handler = McpClientHandler::new(client_info, handle.clone());
        match handler.connect(process).await {
            Ok(svc) => {
                tracing::info!("MCP '{}' connected", server.name);
                services.push(svc);
            }
            Err(e) => {
                tracing::error!("MCP '{}' connect failed: {e}", server.name);
            }
        }
    }

    services
}
