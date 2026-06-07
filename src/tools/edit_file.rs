use rig::completion::request::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::json;

fn default_expected_count() -> usize {
    1
}

#[derive(Deserialize)]
pub struct EditFileArgs {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
    #[serde(default = "default_expected_count")]
    pub expected_count: usize,
}

#[derive(Deserialize, Serialize)]
pub struct EditFile;

impl Tool for EditFile {
    const NAME: &'static str = "edit_file";
    type Error = std::io::Error;
    type Args = EditFileArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "edit_file".to_string(),
            description: "Replace an exact substring in a file. Fails if the match count differs from expected_count (default 1), preventing silent no-ops or accidental mass replacements. Prefer this over write_file for targeted edits.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "path to the file to edit" },
                    "old_text": { "type": "string", "description": "exact text to find and replace" },
                    "new_text": { "type": "string", "description": "replacement text" },
                    "expected_count": { "type": "integer", "description": "expected number of occurrences to replace (default 1)" }
                },
                "required": ["path", "old_text", "new_text"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let contents = std::fs::read_to_string(&args.path)?;
        let count = contents.matches(args.old_text.as_str()).count();
        if count != args.expected_count {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "expected {} occurrence(s) of old_text but found {}",
                    args.expected_count, count
                ),
            ));
        }
        let new_contents = contents.replacen(args.old_text.as_str(), &args.new_text, count);
        std::fs::write(&args.path, new_contents)?;
        Ok(json!({ "replaced": count, "path": args.path }).to_string())
    }
}
