//! Normalizes raw `rig` streaming items into the unified `OurItem`/`OurStream`
//! representation and drives a turn's stream end-to-end: forwarding `AiEvent`s
//! to the UI while handling in-flight `AgentCommand`s (cancel/approve/deny).

use std::collections::HashMap;
use std::pin::Pin;
use std::time::Instant;

use futures::{Stream, StreamExt};
use rig::agent::{MultiTurnStreamItem, StreamingError};
use rig::completion::request::PromptError;
use rig::message::{Message, ToolResultContent};
use rig::streaming::{StreamedAssistantContent, StreamedUserContent};
use tokio::sync::mpsc;

use crate::approval::ApprovalGate;
use crate::memory::ToolCallRecord;
use crate::ui::AiEvent;

use super::AgentCommand;

// ── unified stream item ───────────────────────────────────────────────────────

/// A provider-agnostic streaming event, normalized from `rig`'s
/// `MultiTurnStreamItem<R>` (whose `R` varies per provider/model) by
/// `map_item` so `drive_stream` and the UI never need to match on
/// provider-specific response types.
pub(crate) enum OurItem {
    Text(String),
    History(Vec<Message>),
    Error(String),
    ToolCallStart {
        call_id: String,
        name: String,
        args_json: String,
    },
    ToolCallResult {
        call_id: String,
        result: String,
    },
    /// Rig aborted the stream after `max_turns` turns without a final
    /// response. `history`/`pending_prompt` are rig's own
    /// `MaxTurnsError::chat_history`/`::prompt` — the exact state needed to
    /// resume without re-doing (and re-executing) already-made tool calls.
    MaxTurnsReached {
        history: Vec<Message>,
        pending_prompt: Message,
        max_turns: usize,
    },
}

/// A boxed stream of normalized items — the type `DynAgent::stream_chat`
/// returns, regardless of which provider produced the underlying stream.
pub(crate) type OurStream = Pin<Box<dyn Stream<Item = OurItem> + Send>>;

pub(crate) fn map_item<R>(
    item: Result<MultiTurnStreamItem<R>, rig::agent::StreamingError>,
) -> Option<OurItem> {
    match item {
        Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(t))) => {
            Some(OurItem::Text(t.text))
        }
        Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
            tool_call,
            internal_call_id,
        })) => Some(OurItem::ToolCallStart {
            call_id: internal_call_id,
            name: tool_call.function.name,
            args_json: tool_call.function.arguments.to_string(),
        }),
        Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
            tool_result,
            internal_call_id,
        })) => {
            let result = tool_result
                .content
                .iter()
                .filter_map(|c| match c {
                    ToolResultContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            Some(OurItem::ToolCallResult {
                call_id: internal_call_id,
                result,
            })
        }
        Ok(MultiTurnStreamItem::FinalResponse(fin)) => Some(OurItem::History(
            fin.history().map(|h| h.to_vec()).unwrap_or_default(),
        )),
        Err(StreamingError::Prompt(boxed)) => match *boxed {
            PromptError::MaxTurnsError {
                max_turns,
                chat_history,
                prompt,
            } => Some(OurItem::MaxTurnsReached {
                history: *chat_history,
                pending_prompt: *prompt,
                max_turns,
            }),
            other => Some(OurItem::Error(other.to_string())),
        },
        Err(e) => Some(OurItem::Error(e.to_string())),
        _ => None,
    }
}

// ── stream driver ─────────────────────────────────────────────────────────────

/// Outcome of `drive_stream`: whether the turn ran to completion or was
/// interrupted mid-flight by an `AgentCommand::Cancel`.
pub(crate) enum DriveResult {
    Done,
    Cancelled,
    /// Rig hit its turn cap. `pending_prompt` is the message that still
    /// needs to be sent to resume — `history` (the `&mut` param) has
    /// already been updated to the exact chat history rig had accumulated
    /// at the cutoff, so resuming is `agent.stream_chat(pending_prompt,
    /// history)`, not a fresh turn.
    MaxTurnsReached {
        pending_prompt: Box<Message>,
    },
}

pub(crate) async fn drive_stream(
    mut stream: OurStream,
    ai_tx: &mpsc::UnboundedSender<AiEvent>,
    user_rx: &mut mpsc::UnboundedReceiver<AgentCommand>,
    history: &mut Vec<Message>,
    gate: &ApprovalGate,
    tool_timings: &mut HashMap<String, Instant>,
    tool_records: &mut Vec<ToolCallRecord>,
) -> DriveResult {
    // Buffers (name, args_json) for in-flight calls awaiting their result.
    let mut pending: HashMap<String, (String, String)> = HashMap::new();
    loop {
        tokio::select! {
            item = stream.next() => {
                match item {
                    None => {
                        ai_tx.send(AiEvent::Done).ok();
                        return DriveResult::Done;
                    }
                    Some(OurItem::Text(text)) => {
                        if ai_tx.send(AiEvent::Token(text)).is_err() {
                            return DriveResult::Done;
                        }
                    }
                    Some(OurItem::ToolCallStart { call_id, name, args_json }) => {
                        tool_timings.insert(call_id.clone(), Instant::now());
                        pending.insert(call_id.clone(), (name.clone(), args_json.clone()));
                        ai_tx.send(AiEvent::ToolCallStart {
                            call_id,
                            name,
                            args_json,
                        }).ok();
                    }
                    Some(OurItem::ToolCallResult { call_id, result }) => {
                        let elapsed_ms = tool_timings.remove(&call_id)
                            .map(|t| t.elapsed().as_millis() as u64)
                            .unwrap_or(0);
                        if let Some((name, args_json)) = pending.remove(&call_id) {
                            tool_records.push(ToolCallRecord {
                                name,
                                args_json,
                                result: result.clone(),
                            });
                        }
                        ai_tx.send(AiEvent::ToolCallResult { call_id, result, elapsed_ms }).ok();
                    }
                    Some(OurItem::History(h)) => {
                        *history = h;
                        ai_tx.send(AiEvent::Done).ok();
                        return DriveResult::Done;
                    }
                    Some(OurItem::Error(e)) => {
                        ai_tx.send(AiEvent::Error(e)).ok();
                        return DriveResult::Done;
                    }
                    Some(OurItem::MaxTurnsReached { history: h, pending_prompt, max_turns }) => {
                        *history = h;
                        ai_tx.send(AiEvent::MaxTurnsReached { max_turns }).ok();
                        return DriveResult::MaxTurnsReached {
                            pending_prompt: Box::new(pending_prompt),
                        };
                    }
                }
            }
            cmd = user_rx.recv() => {
                match cmd {
                    Some(AgentCommand::Cancel) => {
                        return DriveResult::Cancelled;
                    }
                    Some(AgentCommand::ApproveToolCall(id)) => {
                        if let Some(tx) = gate.lock().unwrap().remove(&id) {
                            tx.send(true).ok();
                        }
                    }
                    Some(AgentCommand::DenyToolCall(id)) => {
                        if let Some(tx) = gate.lock().unwrap().remove(&id) {
                            tx.send(false).ok();
                        }
                    }
                    None => return DriveResult::Done,
                    // Queue non-approval commands by ignoring them during stream
                    // (model/tools changes take effect after the current turn)
                    Some(_) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_item_turns_max_turns_error_into_max_turns_reached() {
        let history = vec![Message::user("earlier turn")];
        let prompt = Message::user("still pending");
        let err: StreamingError = StreamingError::Prompt(Box::new(PromptError::MaxTurnsError {
            max_turns: 25,
            chat_history: Box::new(history.clone()),
            prompt: Box::new(prompt.clone()),
        }));
        let item: Result<MultiTurnStreamItem<()>, StreamingError> = Err(err);

        match map_item(item) {
            Some(OurItem::MaxTurnsReached {
                history: h,
                pending_prompt,
                max_turns,
            }) => {
                assert_eq!(max_turns, 25);
                assert_eq!(h, history);
                assert_eq!(pending_prompt, prompt);
            }
            _ => panic!("expected OurItem::MaxTurnsReached"),
        }
    }

    #[test]
    fn map_item_other_errors_still_stringify() {
        let err: StreamingError = StreamingError::Prompt(Box::new(PromptError::PromptCancelled {
            chat_history: vec![],
            reason: "boom".to_string(),
        }));
        let item: Result<MultiTurnStreamItem<()>, StreamingError> = Err(err);

        match map_item(item) {
            Some(OurItem::Error(msg)) => assert!(msg.contains("boom")),
            _ => panic!("expected OurItem::Error"),
        }
    }
}
