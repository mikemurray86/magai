use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use rig::completion::ToolDefinition;
use rig::tool::{ToolDyn, ToolError};
use tokio::sync::{mpsc, oneshot};

use crate::hooks::{HookEvent, HookRunner};
use crate::ui::AiEvent;

pub type ApprovalGate = Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>;

#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    Auto,
    #[default]
    AskDangerous,
    AskAlways,
}

pub struct GatedTool {
    inner: Box<dyn ToolDyn>,
    is_dangerous: bool,
    mode: PermissionMode,
    gate: ApprovalGate,
    event_tx: mpsc::UnboundedSender<AiEvent>,
    hook_runner: Arc<HookRunner>,
}

impl GatedTool {
    pub fn new(
        inner: impl ToolDyn + 'static,
        is_dangerous: bool,
        mode: PermissionMode,
        gate: ApprovalGate,
        event_tx: mpsc::UnboundedSender<AiEvent>,
        hook_runner: Arc<HookRunner>,
    ) -> Self {
        Self {
            inner: Box::new(inner),
            is_dangerous,
            mode,
            gate,
            event_tx,
            hook_runner,
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
