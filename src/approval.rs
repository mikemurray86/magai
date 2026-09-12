use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use rig::completion::ToolDefinition;
use rig::tool::{ToolDyn, ToolError};
use tokio::sync::{mpsc, oneshot};

use crate::hooks::{HookEvent, HookRunner};
use crate::ui::AiEvent;

pub type ApprovalGate = Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>;

/// Per-turn tally of `(total, failed, denied)` tool calls, shared across every
/// `GatedTool` and read/reset by `finish_turn` in `ai.rs` after each turn —
/// the implicit quality signal for that turn.
pub type ToolOutcomeCounter = Arc<Mutex<(usize, usize, usize)>>;

#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    Auto,
    #[default]
    AskDangerous,
    AskAlways,
}

/// The shared plumbing every [`GatedTool`] needs, bundled so it can be threaded
/// to the built-in tool registration and to the MCP connector alike.
#[derive(Clone)]
pub struct GateContext {
    pub mode: PermissionMode,
    pub gate: ApprovalGate,
    pub event_tx: mpsc::UnboundedSender<AiEvent>,
    pub hook_runner: Arc<HookRunner>,
    pub tool_outcomes: ToolOutcomeCounter,
}

impl GateContext {
    /// Wrap a tool so its calls pass through the approval gate and the
    /// pre/post-tool-call hooks.
    pub fn wrap(&self, inner: impl ToolDyn + 'static, is_dangerous: bool) -> GatedTool {
        GatedTool::new(
            inner,
            is_dangerous,
            self.mode,
            self.gate.clone(),
            self.event_tx.clone(),
            self.hook_runner.clone(),
            self.tool_outcomes.clone(),
        )
    }
}

pub struct GatedTool {
    inner: Box<dyn ToolDyn>,
    is_dangerous: bool,
    mode: PermissionMode,
    gate: ApprovalGate,
    event_tx: mpsc::UnboundedSender<AiEvent>,
    hook_runner: Arc<HookRunner>,
    tool_outcomes: ToolOutcomeCounter,
}

impl GatedTool {
    pub fn new(
        inner: impl ToolDyn + 'static,
        is_dangerous: bool,
        mode: PermissionMode,
        gate: ApprovalGate,
        event_tx: mpsc::UnboundedSender<AiEvent>,
        hook_runner: Arc<HookRunner>,
        tool_outcomes: ToolOutcomeCounter,
    ) -> Self {
        Self {
            inner: Box::new(inner),
            is_dangerous,
            mode,
            gate,
            event_tx,
            hook_runner,
            tool_outcomes,
        }
    }

    fn needs_approval(&self) -> bool {
        match self.mode {
            PermissionMode::Auto => false,
            PermissionMode::AskDangerous => self.is_dangerous,
            PermissionMode::AskAlways => true,
        }
    }
}

impl ToolDyn for GatedTool {
    fn name(&self) -> String {
        self.inner.name()
    }

    fn definition<'a>(&'a self, prompt: String) -> BoxFuture<'a, ToolDefinition> {
        self.inner.definition(prompt)
    }

    fn call<'a>(&'a self, args: String) -> BoxFuture<'a, Result<String, ToolError>> {
        Box::pin(async move {
            if self.needs_approval() {
                let call_id = uuid::Uuid::new_v4().to_string();
                let (tx, rx) = oneshot::channel::<bool>();
                self.gate.lock().unwrap().insert(call_id.clone(), tx);
                self.event_tx
                    .send(AiEvent::ToolCallApprovalRequired {
                        call_id: call_id.clone(),
                        name: self.inner.name(),
                        args_json: args.clone(),
                        is_dangerous: self.is_dangerous,
                    })
                    .ok();
                match rx.await {
                    Ok(true) => {}
                    _ => {
                        self.gate.lock().unwrap().remove(&call_id);
                        let mut stats = self.tool_outcomes.lock().unwrap();
                        stats.0 += 1;
                        stats.2 += 1;
                        drop(stats);
                        return Err(ToolError::ToolCallError(Box::new(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "tool call denied by user",
                        ))));
                    }
                }
            }

            self.hook_runner.fire(
                HookEvent::PreToolCall,
                HashMap::from([
                    ("MAGAI_TOOL_NAME".to_string(), self.inner.name()),
                    ("MAGAI_TOOL_INPUT".to_string(), args.clone()),
                ]),
            );

            let result = self.inner.call(args.clone()).await;

            let (output, success) = match &result {
                Ok(s) => (s.clone(), "true".to_string()),
                Err(e) => (e.to_string(), "false".to_string()),
            };
            {
                let mut stats = self.tool_outcomes.lock().unwrap();
                stats.0 += 1;
                if result.is_err() {
                    stats.1 += 1;
                }
            }
            self.hook_runner.fire(
                HookEvent::PostToolCall,
                HashMap::from([
                    ("MAGAI_TOOL_NAME".to_string(), self.inner.name()),
                    ("MAGAI_TOOL_INPUT".to_string(), args),
                    ("MAGAI_TOOL_OUTPUT".to_string(), output),
                    ("MAGAI_TOOL_SUCCESS".to_string(), success),
                ]),
            );

            result
        })
    }
}
