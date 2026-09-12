use regex::Regex;
use rig::completion::request::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::json;

fn default_timeout() -> u64 {
    15
}
fn default_max_bytes() -> usize {
    50_000
}

#[derive(Deserialize)]
pub struct WebFetchArgs {
    pub url: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_bytes")]
    pub max_bytes: usize,
}

#[derive(Deserialize, Serialize)]
pub struct WebFetch;

impl Tool for WebFetch {
    const NAME: &'static str = "web_fetch";
    type Error = std::io::Error;
    type Args = WebFetchArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "web_fetch".to_string(),
            description: "Fetch a URL and return its text content. HTML is stripped to readable plain text. Useful for documentation, GitHub issues, and crate pages.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "URL to fetch" },
                    "timeout_secs": { "type": "integer", "description": "request timeout in seconds (default 15)" },
                    "max_bytes": { "type": "integer", "description": "maximum response size in bytes before truncation (default 50000)" }
                },
                "required": ["url"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(args.timeout_secs))
            .user_agent("magai/0.1 (text fetch)")
            .build()
            .map_err(io_err)?;

        let resp = client.get(&args.url).send().await.map_err(io_err)?;
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let bytes = resp.bytes().await.map_err(io_err)?;
        let truncated = bytes.len() > args.max_bytes;
        let raw = String::from_utf8_lossy(if truncated {
            &bytes[..args.max_bytes]
        } else {
            &bytes
        });

        let text = if content_type.contains("text/html") || content_type.is_empty() {
            html_to_text(&raw)
        } else {
            raw.into_owned()
        };

        Ok(json!({
            "url": args.url,
            "status": status,
            "content_type": content_type,
            "text": text,
            "truncated": truncated
        })
        .to_string())
    }
}

fn html_to_text(html: &str) -> String {
    // Remove <script> and <style> blocks entirely (content + tags)
    let script = Regex::new(r"(?is)<(script|style)[^>]*>.*?</(script|style)>").unwrap();
    let s = script.replace_all(html, " ");

    // Replace block-level closing tags with newlines for readability
    let block_end = Regex::new(r"(?i)</(p|div|li|tr|h[1-6]|br|blockquote|pre)>").unwrap();
    let s = block_end.replace_all(&s, "\n");

    // Replace <br> and <br/> with newlines
    let br = Regex::new(r"(?i)<br\s*/?>").unwrap();
    let s = br.replace_all(&s, "\n");

    // Strip remaining tags
    let tags = Regex::new(r"<[^>]+>").unwrap();
    let s = tags.replace_all(&s, "");

    // Decode common HTML entities
    let s = s
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&ndash;", "-")
        .replace("&mdash;", "--")
        .replace("&hellip;", "...");

    // Collapse runs of whitespace / blank lines
    let multi_blank = Regex::new(r"\n{3,}").unwrap();
    let s = multi_blank.replace_all(&s, "\n\n");

    let spaces = Regex::new(r"[ \t]+").unwrap();
    let s = spaces.replace_all(&s, " ");

    s.trim().to_string()
}

fn io_err(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}
