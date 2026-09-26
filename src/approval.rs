use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use rig::completion::ToolDefinition;
use rig::tool::{ToolDyn, ToolError};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::hooks::{HookEvent, HookRunner};
use crate::ui::AiEvent;

pub mod review;

use review::{parse_review, review_input, Review};

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
    /// Dangerous calls are judged by a reviewer model (`[permissions.reviewer]`)
    /// that may allow, deny, suggest an alternative, or defer to the user.
    Smart,
}

/// Whether a tool's calls count as dangerous. `Classify` decides per call from
/// the raw args JSON — `shell_command` uses it so `ls` doesn't prompt like `rm`.
#[derive(Clone)]
pub enum Danger {
    Safe,
    Always,
    Classify(Arc<dyn Fn(&str) -> bool + Send + Sync>),
}

impl From<bool> for Danger {
    fn from(dangerous: bool) -> Self {
        if dangerous {
            Danger::Always
        } else {
            Danger::Safe
        }
    }
}

impl Danger {
    fn check(&self, args: &str) -> bool {
        match self {
            Danger::Safe => false,
            Danger::Always => true,
            Danger::Classify(f) => f(args),
        }
    }
}

/// Asks the reviewer model about one call: prompt in, raw reply out (`None`
/// on timeout/error). A closure rather than `BackgroundModel` so tests can
/// script replies.
pub type ReviewFn = Arc<dyn Fn(String) -> BoxFuture<'static, Option<String>> + Send + Sync>;

/// State for `PermissionMode::Smart`, shared by every gated tool.
pub struct SmartReview {
    reviewer: ReviewFn,
    /// The prompt that started the current turn, shown to the reviewer.
    turn_request: Mutex<String>,
    /// `(tool, args)` the reviewer turned down, with its reason. The same call
    /// coming back goes to the user rather than round the reviewer again.
    denied: Mutex<HashMap<(String, String), String>>,
}

impl SmartReview {
    pub fn new(reviewer: ReviewFn) -> Self {
        Self {
            reviewer,
            turn_request: Mutex::new(String::new()),
            denied: Mutex::new(HashMap::new()),
        }
    }

    pub fn set_turn_request(&self, request: &str) {
        *self.turn_request.lock().unwrap() = request.to_string();
    }

    /// Forget past denials, e.g. when the conversation is cleared.
    pub fn clear(&self) {
        self.denied.lock().unwrap().clear();
    }
}

/// The name of the argument injected into dangerous tools' schemas in smart
/// mode, carrying the model's reason for the call to the reviewer.
const JUSTIFICATION: &str = "justification";

/// The shared plumbing every [`GatedTool`] needs, bundled so it can be threaded
/// to the built-in tool registration and to the MCP connector alike.
#[derive(Clone)]
pub struct GateContext {
    pub mode: PermissionMode,
    pub gate: ApprovalGate,
    pub event_tx: mpsc::UnboundedSender<AiEvent>,
    pub hook_runner: Arc<HookRunner>,
    pub tool_outcomes: ToolOutcomeCounter,
    /// Set when `mode` is `Smart` and a reviewer is configured; without it
    /// `Smart` behaves as `AskDangerous`.
    pub smart: Option<Arc<SmartReview>>,
    /// `[permissions] safe_tools`: tool names never treated as dangerous.
    pub safe_tools: Arc<HashSet<String>>,
}

impl GateContext {
    /// Wrap a tool so its calls pass through the approval gate and the
    /// pre/post-tool-call hooks.
    pub fn wrap(&self, inner: impl ToolDyn + 'static, danger: impl Into<Danger>) -> GatedTool {
        let danger = if self.safe_tools.contains(&inner.name()) {
            Danger::Safe
        } else {
            danger.into()
        };
        GatedTool {
            inner: Box::new(inner),
            danger,
            ctx: self.clone(),
            inner_declares_justification: AtomicBool::new(false),
        }
    }
}

pub struct GatedTool {
    inner: Box<dyn ToolDyn>,
    danger: Danger,
    ctx: GateContext,
    /// Set by `definition` when the tool already has its own `justification`
    /// argument, which must then be passed through rather than stripped.
    inner_declares_justification: AtomicBool,
}

impl GatedTool {
    /// The reviewer, when smart mode is actually in effect for this tool.
    fn smart(&self) -> Option<&SmartReview> {
        match (self.ctx.mode, &self.danger) {
            (PermissionMode::Smart, Danger::Always | Danger::Classify(_)) => {
                self.ctx.smart.as_deref()
            }
            _ => None,
        }
    }

    /// Splits the injected `justification` out of `args`, returning the args
    /// the tool itself should see.
    fn take_justification(&self, args: String) -> (String, Option<String>) {
        if self.smart().is_none() || self.inner_declares_justification.load(Ordering::Relaxed) {
            return (args, None);
        }
        let Ok(Value::Object(mut map)) = serde_json::from_str::<Value>(&args) else {
            return (args, None);
        };
        match map.remove(JUSTIFICATION) {
            Some(j) => {
                let text = j
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| j.to_string());
                (Value::Object(map).to_string(), Some(text))
            }
            None => (args, None),
        }
    }

    /// Count a denied call and build the error the model sees.
    fn deny(&self, msg: String) -> ToolError {
        let mut stats = self.ctx.tool_outcomes.lock().unwrap();
        stats.0 += 1;
        stats.2 += 1;
        ToolError::ToolCallError(Box::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            msg,
        )))
    }

    /// Round-trips the call through the TUI approval card.
    async fn ask_user(
        &self,
        args: &str,
        dangerous: bool,
        review_note: Option<String>,
    ) -> Result<(), ToolError> {
        let call_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel::<bool>();
        self.ctx.gate.lock().unwrap().insert(call_id.clone(), tx);
        self.ctx
            .event_tx
            .send(AiEvent::ToolCallApprovalRequired {
                call_id: call_id.clone(),
                name: self.inner.name(),
                args_json: args.to_string(),
                is_dangerous: dangerous,
                review_note,
            })
            .ok();
        match rx.await {
            Ok(true) => Ok(()),
            _ => {
                self.ctx.gate.lock().unwrap().remove(&call_id);
                Err(self.deny("tool call denied by user".to_string()))
            }
        }
    }

    fn notice(&self, msg: String) {
        self.ctx.event_tx.send(AiEvent::Notice(msg)).ok();
    }

    /// The smart-mode decision for a dangerous call.
    async fn review(
        &self,
        smart: &SmartReview,
        args: &str,
        justification: Option<&str>,
    ) -> Result<(), ToolError> {
        let name = self.inner.name();
        let key = (name.clone(), args.to_string());
        let prior = smart.denied.lock().unwrap().get(&key).cloned();
        if let Some(reason) = prior {
            let note = format!("Resubmitted after the reviewer declined it: {reason}");
            return self.ask_user(args, true, Some(note)).await;
        }

        let request = smart.turn_request.lock().unwrap().clone();
        let reply = (smart.reviewer)(review_input(&request, &name, args, justification)).await;
        match reply.as_deref().and_then(parse_review) {
            None => {
                let note = "The reviewer gave no usable verdict.".to_string();
                self.ask_user(args, true, Some(note)).await
            }
            Some(Review::Allow { reason }) => {
                self.notice(format!("reviewer allowed {name}: {reason}"));
                Ok(())
            }
            Some(Review::AskUser { reason }) => {
                self.ask_user(args, true, Some(format!("Reviewer: {reason}")))
                    .await
            }
            Some(Review::Suggest { reason, suggestion }) => {
                self.notice(format!("reviewer declined {name}: {reason}"));
                smart.denied.lock().unwrap().insert(key, reason.clone());
                Err(self.deny(format!(
                    "denied by reviewer: {reason}. Suggested alternative: {suggestion}"
                )))
            }
            Some(Review::Deny { reason }) => {
                self.notice(format!("reviewer blocked {name} as unsafe: {reason}"));
                smart.denied.lock().unwrap().insert(key, reason.clone());
                Err(self.deny(format!(
                    "denied by reviewer as unsafe: {reason}. Do not retry this call."
                )))
            }
        }
    }

    async fn authorize(&self, args: &str, justification: Option<&str>) -> Result<(), ToolError> {
        let dangerous = self.danger.check(args);
        match self.ctx.mode {
            PermissionMode::Auto => Ok(()),
            PermissionMode::AskAlways => self.ask_user(args, dangerous, None).await,
            _ if !dangerous => Ok(()),
            PermissionMode::Smart => match self.smart() {
                Some(smart) => self.review(smart, args, justification).await,
                None => self.ask_user(args, true, None).await,
            },
            PermissionMode::AskDangerous => self.ask_user(args, true, None).await,
        }
    }
}

/// Adds a required `justification` string to a JSON-schema object. Returns
/// `false` (leaving the schema alone) when it already has one.
fn inject_justification(parameters: &mut Value) -> bool {
    let Value::Object(schema) = parameters else {
        return true;
    };
    let props = schema
        .entry("properties")
        .or_insert_with(|| Value::Object(Default::default()));
    let Value::Object(props) = props else {
        return true;
    };
    if props.contains_key(JUSTIFICATION) {
        return false;
    }
    props.insert(
        JUSTIFICATION.to_string(),
        serde_json::json!({
            "type": "string",
            "description": "Why this call is needed for the user's request and why it is safe. \
                            Reviewed before the call runs."
        }),
    );
    schema.entry("type").or_insert_with(|| "object".into());
    if let Value::Array(required) = schema
        .entry("required")
        .or_insert_with(|| Value::Array(Vec::new()))
    {
        required.push(JUSTIFICATION.into());
    }
    true
}

impl ToolDyn for GatedTool {
    fn name(&self) -> String {
        self.inner.name()
    }

    fn definition<'a>(&'a self, prompt: String) -> BoxFuture<'a, ToolDefinition> {
        Box::pin(async move {
            let mut def = self.inner.definition(prompt).await;
            if self.smart().is_some() {
                let injected = inject_justification(&mut def.parameters);
                self.inner_declares_justification
                    .store(!injected, Ordering::Relaxed);
            }
            def
        })
    }

    fn call<'a>(&'a self, args: String) -> BoxFuture<'a, Result<String, ToolError>> {
        Box::pin(async move {
            let (args, justification) = self.take_justification(args);
            self.authorize(&args, justification.as_deref()).await?;

            self.ctx.hook_runner.fire(
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
                let mut stats = self.ctx.tool_outcomes.lock().unwrap();
                stats.0 += 1;
                if result.is_err() {
                    stats.1 += 1;
                }
            }
            self.ctx.hook_runner.fire(
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

#[cfg(test)]
mod tests {
    use super::*;
    use rig::tool::Tool;

    /// Echoes the args it was actually called with.
    struct Probe;

    impl Tool for Probe {
        const NAME: &'static str = "probe";
        type Error = std::io::Error;
        type Args = Value;
        type Output = String;

        async fn definition(&self, _prompt: String) -> ToolDefinition {
            ToolDefinition {
                name: "probe".to_string(),
                description: String::new(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "cmd": { "type": "string" } },
                    "required": ["cmd"]
                }),
            }
        }

        async fn call(&self, args: Value) -> Result<String, std::io::Error> {
            Ok(args.to_string())
        }
    }

    fn context(
        mode: PermissionMode,
        replies: Vec<&'static str>,
    ) -> (GateContext, mpsc::UnboundedReceiver<AiEvent>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let replies = Arc::new(Mutex::new(replies.into_iter()));
        let reviewer: ReviewFn = Arc::new(move |_| {
            let reply = replies.lock().unwrap().next().map(str::to_owned);
            Box::pin(async move { reply })
        });
        let ctx = GateContext {
            mode,
            gate: Arc::new(Mutex::new(HashMap::new())),
            event_tx,
            hook_runner: Arc::new(HookRunner::new(Vec::new())),
            tool_outcomes: Arc::new(Mutex::new((0, 0, 0))),
            smart: Some(Arc::new(SmartReview::new(reviewer))),
            safe_tools: Default::default(),
        };
        (ctx, event_rx)
    }

    fn approval_requested(events: &mut mpsc::UnboundedReceiver<AiEvent>) -> Option<AiEvent> {
        while let Ok(ev) = events.try_recv() {
            if matches!(ev, AiEvent::ToolCallApprovalRequired { .. }) {
                return Some(ev);
            }
        }
        None
    }

    fn classify_rm() -> Danger {
        Danger::Classify(Arc::new(|args: &str| args.contains("rm")))
    }

    #[tokio::test]
    async fn ask_dangerous_only_prompts_for_dangerous_calls() {
        let (ctx, mut events) = context(PermissionMode::AskDangerous, vec![]);
        let tool = ctx.wrap(Probe, classify_rm());
        assert!(tool.call(r#"{"cmd":"ls"}"#.into()).await.is_ok());
        assert!(approval_requested(&mut events).is_none());

        let pending = tokio::spawn(async move { tool.call(r#"{"cmd":"rm"}"#.into()).await });
        tokio::task::yield_now().await;
        assert!(ctx.gate.lock().unwrap().len() == 1, "rm waits on the gate");
        pending.abort();
    }

    #[tokio::test]
    async fn safe_tools_bypass_the_gate() {
        let (mut ctx, mut events) = context(PermissionMode::AskDangerous, vec![]);
        ctx.safe_tools = Arc::new(HashSet::from(["probe".to_string()]));
        let tool = ctx.wrap(Probe, true);
        assert!(tool.call(r#"{"cmd":"rm"}"#.into()).await.is_ok());
        assert!(approval_requested(&mut events).is_none());
    }

    #[tokio::test]
    async fn smart_mode_injects_and_strips_justification() {
        let (ctx, _events) = context(
            PermissionMode::Smart,
            vec![r#"{"decision":"allow","reason":"ok"}"#],
        );
        let tool = ctx.wrap(Probe, true);
        let def = tool.definition(String::new()).await;
        assert!(def.parameters["properties"]["justification"].is_object());
        assert!(def.parameters["required"]
            .as_array()
            .unwrap()
            .contains(&"justification".into()));

        let out = tool
            .call(r#"{"cmd":"rm","justification":"cleanup"}"#.into())
            .await
            .unwrap();
        assert_eq!(
            out, r#"{"cmd":"rm"}"#,
            "inner tool never sees the justification"
        );
    }

    #[tokio::test]
    async fn smart_mode_denial_then_resubmission_goes_to_the_user() {
        let (ctx, mut events) = context(
            PermissionMode::Smart,
            vec![r#"{"decision":"deny","reason":"destroys data"}"#],
        );
        let tool = Arc::new(ctx.wrap(Probe, true));
        let err = tool
            .call(r#"{"cmd":"rm","justification":"a"}"#.into())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("destroys data"));
        assert!(approval_requested(&mut events).is_none());

        // Same call, different justification: straight to the user.
        let again = tokio::spawn({
            let tool = Arc::clone(&tool);
            async move {
                tool.call(r#"{"cmd":"rm","justification":"b"}"#.into())
                    .await
            }
        });
        let call_id = loop {
            tokio::task::yield_now().await;
            if let Some(AiEvent::ToolCallApprovalRequired {
                call_id,
                review_note,
                ..
            }) = approval_requested(&mut events)
            {
                assert!(review_note.unwrap().contains("destroys data"));
                break call_id;
            }
        };
        let approve = ctx.gate.lock().unwrap().remove(&call_id).unwrap();
        approve.send(true).ok();
        assert!(again.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn smart_mode_suggestion_is_returned_to_the_model() {
        let (ctx, _events) = context(
            PermissionMode::Smart,
            vec![r#"{"decision":"suggest","reason":"too broad","suggestion":"rm one file"}"#],
        );
        let tool = ctx.wrap(Probe, true);
        let err = tool.call(r#"{"cmd":"rm"}"#.into()).await.unwrap_err();
        assert!(err.to_string().contains("rm one file"));
        assert_eq!(*ctx.tool_outcomes.lock().unwrap(), (1, 0, 1));
    }

    #[tokio::test]
    async fn smart_mode_without_a_verdict_asks_the_user() {
        let (ctx, mut events) = context(PermissionMode::Smart, vec!["no idea"]);
        let tool = ctx.wrap(Probe, true);
        let pending = tokio::spawn(async move { tool.call(r#"{"cmd":"rm"}"#.into()).await });
        let mut asked = None;
        for _ in 0..10 {
            tokio::task::yield_now().await;
            asked = approval_requested(&mut events);
            if asked.is_some() {
                break;
            }
        }
        assert!(asked.is_some());
        pending.abort();
    }
}
