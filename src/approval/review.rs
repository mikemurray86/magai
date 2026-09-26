//! The `smart` permission mode's reviewer: its prompt, the text it is shown
//! for each call, and parsing of its verdict. Pure, so the gate logic in
//! `approval.rs` can be tested without a model behind it.

use serde_json::Value;

pub const DEFAULT_REVIEWER_PROMPT: &str = "\
You are a security reviewer gating the tool calls of an autonomous coding agent \
working inside a user's project directory. For each call you are shown the \
user's request, the tool, its arguments, and the agent's justification. Decide:

- \"allow\": clearly needed for the request and safe (confined to the project, \
reversible, no secrets leaving the machine, no system-wide changes).
- \"suggest\": the goal is reasonable but there is a safer or more targeted way; \
put that alternative in \"suggestion\".
- \"ask_user\": plausibly fine but risky or ambiguous enough that the user \
should decide (deleting files, pushing, installing packages, network writes).
- \"deny\": unsafe or unrelated to the request (destroying data outside the \
project, exfiltrating credentials, disabling security, privilege escalation).

Reply with a single JSON object and nothing else:
{\"decision\": \"allow\" | \"suggest\" | \"ask_user\" | \"deny\", \"reason\": \"<one sentence>\", \"suggestion\": \"<only for suggest>\"}";

/// The reviewer's verdict on one tool call.
#[derive(Debug, Clone, PartialEq)]
pub enum Review {
    Allow { reason: String },
    Suggest { reason: String, suggestion: String },
    AskUser { reason: String },
    Deny { reason: String },
}

pub fn review_input(
    user_request: &str,
    tool: &str,
    args_json: &str,
    justification: Option<&str>,
) -> String {
    let request = if user_request.trim().is_empty() {
        "(unknown)"
    } else {
        user_request
    };
    let justification = justification
        .filter(|j| !j.trim().is_empty())
        .unwrap_or("(none given)");
    format!(
        "User request:\n{request}\n\nTool: {tool}\nArguments: {args_json}\n\n\
         Agent's justification:\n{justification}"
    )
}

/// Parses the reviewer's reply, tolerating prose or code fences around the
/// object (same fallback as `memory::quality::parse_verdict`). `None` for an
/// unknown decision, or a `suggest` with no suggestion.
pub fn parse_review(response: &str) -> Option<Review> {
    let response = response.trim();
    let parsed: Value = serde_json::from_str(response).unwrap_or_else(|_| {
        let start = response.find('{').unwrap_or(0);
        let end = response.rfind('}').map(|i| i + 1).unwrap_or(0);
        if end > start {
            serde_json::from_str(&response[start..end]).unwrap_or(Value::Null)
        } else {
            Value::Null
        }
    });
    let text = |key: &str| {
        parsed[key]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let reason = text("reason").unwrap_or_else(|| "no reason given".to_string());
    let decision = text("decision")?.to_ascii_lowercase().replace('-', "_");
    Some(match decision.as_str() {
        "allow" => Review::Allow { reason },
        "suggest" => Review::Suggest {
            reason,
            suggestion: text("suggestion")?,
        },
        "ask_user" | "ask" => Review::AskUser { reason },
        "deny" => Review::Deny { reason },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_decision() {
        assert_eq!(
            parse_review(r#"{"decision":"allow","reason":"fine"}"#),
            Some(Review::Allow {
                reason: "fine".into()
            })
        );
        assert_eq!(
            parse_review(r#"{"decision":"suggest","reason":"r","suggestion":"use git_diff"}"#),
            Some(Review::Suggest {
                reason: "r".into(),
                suggestion: "use git_diff".into()
            })
        );
        assert_eq!(
            parse_review(r#"{"decision":"ask-user","reason":"r"}"#),
            Some(Review::AskUser { reason: "r".into() })
        );
        assert_eq!(
            parse_review(r#"{"decision":"DENY"}"#),
            Some(Review::Deny {
                reason: "no reason given".into()
            })
        );
    }

    #[test]
    fn tolerates_fences_and_prose() {
        let reply = "Sure.\n```json\n{\"decision\": \"allow\", \"reason\": \"ok\"}\n```";
        assert!(matches!(parse_review(reply), Some(Review::Allow { .. })));
    }

    #[test]
    fn rejects_unusable_replies() {
        assert_eq!(parse_review("looks fine to me"), None);
        assert_eq!(parse_review(r#"{"decision":"maybe"}"#), None);
        assert_eq!(parse_review(r#"{"decision":"suggest","reason":"r"}"#), None);
    }

    #[test]
    fn input_fills_in_missing_context() {
        let s = review_input("", "shell_command", "{}", None);
        assert!(s.contains("(unknown)") && s.contains("(none given)"));
    }
}
