use rig::completion::request::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::Read;

#[derive(Deserialize)]
pub struct ReaderArgs {
    pub file_name: String,
}

#[derive(Deserialize, Serialize)]
pub struct ReadFile;

impl Tool for ReadFile {
    const NAME: &'static str = "read_file";
    type Error = std::io::Error;
    type Args = ReaderArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "read_file".to_string(),
            description: "Read the contents of a file.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_name": { "type": "string", "description": "the file to read" }
                }
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let file = std::fs::File::open(args.file_name)?;
        let mut buf_reader = std::io::BufReader::new(file);
        let mut contents = String::new();
        buf_reader.read_to_string(&mut contents)?;
        Ok(contents)
    }
}
