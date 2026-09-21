//! Owns the ratatui event loop (`App::run`): the `App` state, the
//! `AiEvent`/`ChatMessage` types, and the bridge from `ai::run_agent` events
//! to on-screen state. See `render` for drawing, `input` for event handling,
//! and `text` for pure formatting/wrapping helpers.

mod input;
mod render;
pub(crate) mod text;

use ratatui::{
    style::{Color, Modifier, Style},
    DefaultTerminal,
};
use ratatui_textarea::TextArea;
use tokio::sync::mpsc;

use text::{format_tool_call, summarize_result};

pub enum AiEvent {
    ResponseStart(String),
    Token(String),
    Done,
    Error(String),
    /// An informational success message. `Error` is for failures only — it
    /// used to carry both, so successful undos rendered as errors.
    Notice(String),
    ModelList {
        provider: String,
        models: Vec<String>,
    },
    /// Rendered `/mcp` report: configured servers and their connection state.
    McpStatus(String),
    ToolCallStart {
        call_id: String,
        name: String,
        args_json: String,
    },
    ToolCallResult {
        call_id: String,
        result: String,
        elapsed_ms: u64,
    },
    ToolCallApprovalRequired {
        call_id: String,
        name: String,
        args_json: String,
        is_dangerous: bool,
    },
    ContextTruncated {
        turns_dropped: usize,
    },
    HistoryCleared,
    /// Rendered `/checkpoints` table.
    CheckpointList(String),
    /// Unified diff text for `/diff`, rendered without markdown.
    CheckpointDiff(String),
    /// A destructive checkpoint action awaiting confirmation. `blocked` is
    /// `Some(reason)` when it cannot proceed.
    CheckpointPreview {
        action: crate::checkpoint::CheckpointAction,
        title: String,
        lines: Vec<String>,
        blocked: Option<String>,
    },
    MaxTurnsReached {
        max_turns: usize,
    },
    /// Sent after a finished turn's transcript has been persisted (only when
    /// `[quality] enabled = true`), so the UI can attach a `/rate` rating to
    /// it without re-deriving turn identity.
    TurnRecorded {
        turn_id: String,
    },
}

/// A destructive checkpoint action waiting on the user's confirmation.
#[derive(Debug, Clone)]
pub(super) struct CheckpointPrompt {
    pub action: crate::checkpoint::CheckpointAction,
    pub title: String,
    pub lines: Vec<String>,
    /// When set, the action cannot proceed and the card only dismisses.
    pub blocked: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum Role {
    User,
    Assistant,
    System,
    /// A unified diff: rendered one source line per row, truncated rather than
    /// wrapped, and coloured by leading character.
    Diff,
    ToolCall,
    ToolResult,
}

#[derive(Debug, Clone)]
struct ChatMessage {
    role: Role,
    content: String,
    model_name: Option<String>,
    call_id: Option<String>,
    elapsed_ms: Option<u64>,
}

#[derive(Debug, Clone)]
struct PendingApproval {
    call_id: String,
    name: String,
    args_json: String,
    is_dangerous: bool,
}

pub struct App {
    messages: Vec<ChatMessage>,
    textarea: TextArea<'static>,
    exit: bool,
    is_waiting: bool,
    user_tx: mpsc::UnboundedSender<crate::ai::AgentCommand>,
    ai_rx: mpsc::UnboundedReceiver<AiEvent>,
    scroll: u16,
    content_lines: u16,
    view_height: u16,
    view_width: u16,
    auto_scroll: bool,
    current_model: String,
    pending_approval: Option<PendingApproval>,
    pending_model: Option<String>,
    input_history: Vec<String>,
    history_cursor: Option<usize>,
    skills: Vec<crate::skills::Skill>,
    plugins: Vec<crate::plugins::Plugin>,
    spinner_frame: u8,
    config: crate::config::Config,
    model_ac_idx: Option<usize>,
    provider_ac_idx: Option<usize>,
    cmd_ac_idx: Option<usize>,
    provider_models: Option<(String, Vec<String>)>,
    provider_model_sel: usize,
    pending_checkpoint: Option<CheckpointPrompt>,
    pending_max_turns: Option<usize>,
    max_turns_input: String,
    last_turn_id: Option<String>,
}

impl App {
    pub fn new(
        user_tx: mpsc::UnboundedSender<crate::ai::AgentCommand>,
        ai_rx: mpsc::UnboundedReceiver<AiEvent>,
        config: crate::config::Config,
        current_model: String,
    ) -> Self {
        let plugins = crate::plugins::discover();
        let plugin_skills = crate::plugins::extract_skills(&plugins);
        let mut skills = crate::skills::discover();
        skills.extend(plugin_skills);

        Self {
            messages: Vec::new(),
            textarea: make_textarea("", false),
            exit: false,
            is_waiting: false,
            user_tx,
            ai_rx,
            scroll: 0,
            content_lines: 0,
            view_height: 24,
            view_width: 80,
            auto_scroll: true,
            current_model,
            pending_approval: None,
            pending_model: None,
            input_history: Vec::new(),
            history_cursor: None,
            skills,
            plugins,
            spinner_frame: 0,
            config,
            model_ac_idx: None,
            provider_ac_idx: None,
            cmd_ac_idx: None,
            provider_models: None,
            provider_model_sel: 0,
            pending_checkpoint: None,
            pending_max_turns: None,
            max_turns_input: String::new(),
            last_turn_id: None,
        }
    }

    pub fn run(mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        while !self.exit {
            self.poll_ai_events();
            terminal.draw(|frame| self.draw(frame))?;
            self.handle_events()?;
        }
        Ok(())
    }

    fn poll_ai_events(&mut self) {
        while let Ok(event) = self.ai_rx.try_recv() {
            match event {
                AiEvent::ResponseStart(model) => {
                    self.current_model = model.clone();
                    self.pending_model = Some(model);
                }
                AiEvent::Token(token) => {
                    if let Some(last) = self.messages.last_mut() {
                        if last.role == Role::Assistant {
                            last.content.push_str(&token);
                            continue;
                        }
                    }
                    // First token: create the message, claiming the pending model name
                    self.messages.push(ChatMessage {
                        role: Role::Assistant,
                        content: token,
                        model_name: self.pending_model.take(),
                        call_id: None,
                        elapsed_ms: None,
                    });
                }
                AiEvent::Done => {
                    self.is_waiting = false;
                    self.pending_approval = None;
                    self.pending_model = None;
                }
                AiEvent::Error(e) => {
                    self.messages.push(ChatMessage {
                        role: Role::System,
                        content: format!("error: {e}"),
                        model_name: None,
                        call_id: None,
                        elapsed_ms: None,
                    });
                    self.is_waiting = false;
                    self.pending_approval = None;
                }
                AiEvent::ToolCallStart {
                    call_id,
                    name,
                    args_json,
                } => {
                    self.messages.push(ChatMessage {
                        role: Role::ToolCall,
                        content: format_tool_call(&name, &args_json),
                        model_name: None,
                        call_id: Some(call_id),
                        elapsed_ms: None,
                    });
                    self.auto_scroll = true;
                }
                AiEvent::ToolCallResult {
                    call_id,
                    result,
                    elapsed_ms,
                } => {
                    // Update matching ToolCall message with result inline, or add ToolResult
                    let found = self.messages.iter_mut().rev().find(|m| {
                        m.role == Role::ToolCall && m.call_id.as_deref() == Some(&call_id)
                    });
                    if let Some(msg) = found {
                        msg.elapsed_ms = Some(elapsed_ms);
                    }
                    // Show result as a separate message
                    let summary = summarize_result(&result);
                    self.messages.push(ChatMessage {
                        role: Role::ToolResult,
                        content: summary,
                        model_name: None,
                        call_id: Some(call_id),
                        elapsed_ms: Some(elapsed_ms),
                    });
                    self.auto_scroll = true;
                }
                AiEvent::ToolCallApprovalRequired {
                    call_id,
                    name,
                    args_json,
                    is_dangerous,
                } => {
                    self.pending_approval = Some(PendingApproval {
                        call_id,
                        name,
                        args_json,
                        is_dangerous,
                    });
                    self.auto_scroll = true;
                }
                AiEvent::ContextTruncated { turns_dropped } => {
                    self.messages.push(ChatMessage {
                        role: Role::System,
                        content: format!("[context trimmed: dropped {turns_dropped} old turns]"),
                        model_name: None,
                        call_id: None,
                        elapsed_ms: None,
                    });
                }
                AiEvent::HistoryCleared => {
                    self.messages.clear();
                    self.messages.push(ChatMessage {
                        role: Role::System,
                        content: "conversation cleared".into(),
                        model_name: None,
                        call_id: None,
                        elapsed_ms: None,
                    });
                }
                AiEvent::ModelList { provider, models } => {
                    let count = models.len();
                    self.push_system(format!("{count} models from {provider} — ↑↓ navigate  Tab fill  Enter use  Esc dismiss"));
                    self.provider_models = Some((provider, models));
                    self.provider_model_sel = 0;
                    self.textarea = make_textarea("", false);
                    self.model_ac_idx = None;
                }
                AiEvent::McpStatus(report) => {
                    self.push_system(report);
                }
                AiEvent::Notice(msg) => {
                    self.push_system(msg);
                }
                AiEvent::CheckpointList(report) => {
                    self.push_system(report);
                }
                AiEvent::CheckpointDiff(text) => {
                    self.push_diff(text);
                }
                AiEvent::CheckpointPreview {
                    action,
                    title,
                    lines,
                    blocked,
                } => {
                    self.pending_checkpoint = Some(CheckpointPrompt {
                        action,
                        title,
                        lines,
                        blocked,
                    });
                    self.auto_scroll = true;
                }
                AiEvent::MaxTurnsReached { max_turns } => {
                    self.is_waiting = false;
                    self.pending_max_turns = Some(max_turns);
                    self.max_turns_input.clear();
                    self.auto_scroll = true;
                }
                AiEvent::TurnRecorded { turn_id } => {
                    self.last_turn_id = Some(turn_id);
                }
            }
        }
    }

    /// Slash commands (built-in + skills) matching the typed prefix. Only
    /// active while the first line is a single `/word` — once an argument is
    /// being typed the `/model` and `/provider` completions take over.
    fn cmd_ac_candidates(&self) -> Vec<(String, String)> {
        if self.textarea.lines().len() != 1 || self.provider_models.is_some() {
            return vec![];
        }
        let first_line = self.textarea.lines().first().cloned().unwrap_or_default();
        if !first_line.starts_with('/') || first_line.contains(char::is_whitespace) {
            return vec![];
        }
        crate::slash_commands::matching_commands(&first_line, &self.skills)
    }

    fn model_ac_candidates(&self) -> Vec<&crate::config::NamedModel> {
        let first_line = self.textarea.lines().first().cloned().unwrap_or_default();
        let Some(partial) = first_line.strip_prefix("/model ") else {
            return vec![];
        };
        self.config
            .named_models
            .iter()
            .filter(|m| m.alias.starts_with(partial))
            .collect()
    }

    fn provider_ac_candidates(&self) -> Vec<String> {
        let first_line = self.textarea.lines().first().cloned().unwrap_or_default();
        let Some(partial) = first_line.strip_prefix("/provider ") else {
            return vec![];
        };
        // Already sorted, and includes "ollama" only where it is the implicit
        // provider — it used to be advertised even in hosted-only configs.
        let mut names = self.config.provider_names();
        names.retain(|n| n.starts_with(partial));
        names
    }

    fn provider_model_filtered(&self) -> Vec<&str> {
        let Some((_, ref models)) = self.provider_models else {
            return vec![];
        };
        let filter = self
            .textarea
            .lines()
            .first()
            .map(|s| s.as_str())
            .unwrap_or("");
        models
            .iter()
            .filter(|m| filter.is_empty() || m.contains(filter))
            .map(|m| m.as_str())
            .collect()
    }

    fn push_system(&mut self, content: String) {
        self.messages.push(ChatMessage {
            role: Role::System,
            content,
            model_name: None,
            call_id: None,
            elapsed_ms: None,
        });
        self.auto_scroll = true;
    }

    /// Pushes raw diff text, which renders unwrapped and coloured rather than
    /// word-wrapped like an ordinary system message.
    fn push_diff(&mut self, content: String) {
        self.messages.push(ChatMessage {
            role: Role::Diff,
            content,
            model_name: None,
            call_id: None,
            elapsed_ms: None,
        });
        self.auto_scroll = true;
    }
}

fn make_textarea(text: &str, waiting: bool) -> TextArea<'static> {
    let mut ta = if text.is_empty() {
        TextArea::default()
    } else {
        TextArea::new(vec![text.to_string()])
    };
    ta.set_cursor_line_style(Style::default());
    ta.set_placeholder_text("  ❯  message…  Shift+Enter for newline");
    ta.set_placeholder_style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    );
    if waiting {
        ta.set_style(
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        );
    }
    ta
}
