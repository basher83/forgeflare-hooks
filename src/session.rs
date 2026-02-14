use crate::api::{ContentBlock, Message, Usage};
use chrono::Utc;
use serde::Serialize;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Serialize)]
struct JsonlLine<'a> {
    #[serde(rename = "type")]
    turn_type: &'a str,
    #[serde(rename = "sessionId")]
    session_id: &'a str,
    uuid: String,
    #[serde(rename = "parentUuid")]
    parent_uuid: Option<String>,
    timestamp: String,
    cwd: &'a str,
    version: &'a str,
    message: MessagePayload<'a>,
}

#[derive(Serialize)]
struct MessagePayload<'a> {
    role: &'a str,
    content: &'a [ContentBlock],
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<&'a Usage>,
}

pub struct SessionWriter {
    session_id: String,
    dir: PathBuf,
    cwd: String,
    last_uuid: Option<String>,
    prompt_written: bool,
    tool_actions: Vec<(String, String)>,
    model: String,
    start_time: String,
}

impl SessionWriter {
    pub fn new(cwd: &str, model: &str) -> Self {
        let date = Utc::now().format("%Y-%m-%d").to_string();
        let session_id = format!("{}-{}", date, Uuid::new_v4());
        let dir = Path::new(".entire").join("metadata").join(&session_id);

        Self {
            session_id,
            dir,
            cwd: cwd.to_string(),
            last_uuid: None,
            prompt_written: false,
            tool_actions: Vec::new(),
            model: model.to_string(),
            start_time: Utc::now().to_rfc3339(),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn append_user_turn(&mut self, message: &Message) {
        self.collect_tool_actions(message);
        self.append_line("user", message, None);
    }

    pub fn append_assistant_turn(&mut self, message: &Message, usage: &Usage) {
        self.collect_tool_actions(message);
        self.append_line("assistant", message, Some(usage));
    }

    pub fn write_prompt(&mut self, prompt: &str) {
        if self.prompt_written {
            return;
        }
        self.prompt_written = true;

        if let Err(e) = self.ensure_dir() {
            eprintln!("[session] Failed to create directory: {e}");
            return;
        }

        let path = self.dir.join("prompt.txt");
        if let Err(e) = fs::write(&path, prompt) {
            eprintln!("[session] Failed to write prompt.txt: {e}");
        }
    }

    pub fn write_context(&self) {
        if let Err(e) = self.ensure_dir() {
            eprintln!("[session] Failed to create directory: {e}");
            return;
        }

        let mut content = format!(
            "# Session Context\n\n\
             - Session ID: {}\n\
             - Model: {}\n\
             - Start: {}\n\
             - CWD: {}\n",
            self.session_id, self.model, self.start_time, self.cwd
        );

        if !self.tool_actions.is_empty() {
            content.push_str("\n## Key Actions\n\n");
            for (name, arg) in &self.tool_actions {
                content.push_str(&format!("- **{name}**: {arg}\n"));
            }
        }

        let path = self.dir.join("context.md");
        if let Err(e) = fs::write(&path, content) {
            eprintln!("[session] Failed to write context.md: {e}");
        }
    }

    fn append_line(&mut self, turn_type: &str, message: &Message, usage: Option<&Usage>) {
        if let Err(e) = self.ensure_dir() {
            eprintln!("[session] Failed to create directory: {e}");
            return;
        }

        let line_uuid = Uuid::new_v4().to_string();
        let parent_uuid = self.last_uuid.clone();

        let line = JsonlLine {
            turn_type,
            session_id: &self.session_id,
            uuid: line_uuid.clone(),
            parent_uuid,
            timestamp: Utc::now().to_rfc3339(),
            cwd: &self.cwd,
            version: env!("CARGO_PKG_VERSION"),
            message: MessagePayload {
                role: &message.role,
                content: &message.content,
                usage,
            },
        };

        self.last_uuid = Some(line_uuid);

        let path = self.dir.join("full.jsonl");
        let json = match serde_json::to_string(&line) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("[session] Failed to serialize JSONL line: {e}");
                return;
            }
        };

        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(mut f) => {
                if let Err(e) = writeln!(f, "{json}") {
                    eprintln!("[session] Failed to append to full.jsonl: {e}");
                }
            }
            Err(e) => {
                eprintln!("[session] Failed to open full.jsonl: {e}");
            }
        }
    }

    fn ensure_dir(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.dir)
    }

    fn collect_tool_actions(&mut self, message: &Message) {
        for block in &message.content {
            if let ContentBlock::ToolUse { name, input, .. } = block {
                let first_arg = extract_first_arg(input);
                self.tool_actions.push((name.clone(), first_arg));
            }
        }
    }
}

fn extract_first_arg(input: &serde_json::Value) -> String {
    if let Some(obj) = input.as_object() {
        if let Some((_, val)) = obj.iter().next() {
            let s = match val {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            // Truncate long values
            if s.len() > 80 {
                format!("{}...", &s[..s.floor_char_boundary(80)])
            } else {
                s
            }
        } else {
            "{}".to_string()
        }
    } else {
        input.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ContentBlock;
    use std::io::BufRead;

    #[test]
    fn session_id_format() {
        let writer = SessionWriter::new("/tmp", "claude-opus-4-6");
        let id = writer.session_id();
        // Format: YYYY-MM-DD-uuid
        let re = regex_lite(id);
        assert!(re, "session ID should match date-uuid format: {id}");
    }

    fn regex_lite(id: &str) -> bool {
        let parts: Vec<&str> = id.splitn(4, '-').collect();
        if parts.len() < 4 {
            return false;
        }
        // First part: YYYY
        if parts[0].len() != 4 || parts[0].parse::<u32>().is_err() {
            return false;
        }
        // Second part: MM
        if parts[1].len() != 2 || parts[1].parse::<u32>().is_err() {
            return false;
        }
        // Third part: DD
        if parts[2].len() != 2 || parts[2].parse::<u32>().is_err() {
            return false;
        }
        // Remaining is UUID (contains hyphens)
        let uuid_part = &id[11..]; // skip "YYYY-MM-DD-"
        uuid::Uuid::parse_str(uuid_part).is_ok()
    }

    #[test]
    fn jsonl_incremental_write() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();

        // Override the session dir by constructing manually
        let mut writer = SessionWriter::new(cwd, "test-model");
        writer.dir = dir.path().join("session-test");

        let msg = Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text {
                text: "hello".to_string(),
            }],
        };
        writer.append_user_turn(&msg);

        let jsonl_path = writer.dir.join("full.jsonl");
        assert!(
            jsonl_path.exists(),
            "full.jsonl should exist after first write"
        );

        let lines: Vec<String> = std::io::BufReader::new(fs::File::open(&jsonl_path).unwrap())
            .lines()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(lines.len(), 1);

        // Verify it's valid JSON
        let parsed: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(parsed["type"], "user");
        assert_eq!(parsed["message"]["role"], "user");
        assert!(
            parsed["parentUuid"].is_null(),
            "first line has null parentUuid"
        );
    }

    #[test]
    fn parent_uuid_chaining() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = SessionWriter::new(dir.path().to_str().unwrap(), "test-model");
        writer.dir = dir.path().join("session-chain");

        let user_msg = Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text {
                text: "hello".to_string(),
            }],
        };
        writer.append_user_turn(&user_msg);

        let assistant_msg = Message {
            role: "assistant".to_string(),
            content: vec![ContentBlock::Text {
                text: "hi there".to_string(),
            }],
        };
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };
        writer.append_assistant_turn(&assistant_msg, &usage);

        let jsonl_path = writer.dir.join("full.jsonl");
        let lines: Vec<String> = std::io::BufReader::new(fs::File::open(&jsonl_path).unwrap())
            .lines()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(lines.len(), 2);

        let first: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        let second: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();

        // First line: parentUuid is null
        assert!(first["parentUuid"].is_null());
        // Second line: parentUuid equals first line's uuid
        assert_eq!(second["parentUuid"], first["uuid"]);
    }

    #[test]
    fn assistant_turn_includes_usage() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = SessionWriter::new(dir.path().to_str().unwrap(), "test-model");
        writer.dir = dir.path().join("session-usage");

        let msg = Message {
            role: "assistant".to_string(),
            content: vec![ContentBlock::Text {
                text: "response".to_string(),
            }],
        };
        let usage = Usage {
            input_tokens: 1200,
            output_tokens: 350,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 800,
        };
        writer.append_assistant_turn(&msg, &usage);

        let jsonl_path = writer.dir.join("full.jsonl");
        let line = fs::read_to_string(&jsonl_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();

        assert_eq!(parsed["message"]["usage"]["input_tokens"], 1200);
        assert_eq!(parsed["message"]["usage"]["output_tokens"], 350);
        assert_eq!(parsed["message"]["usage"]["cache_read_input_tokens"], 800);
    }

    #[test]
    fn prompt_txt_written_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = SessionWriter::new(dir.path().to_str().unwrap(), "test-model");
        writer.dir = dir.path().join("session-prompt");

        writer.write_prompt("first prompt");
        writer.write_prompt("second prompt should be ignored");

        let prompt_path = writer.dir.join("prompt.txt");
        let content = fs::read_to_string(prompt_path).unwrap();
        assert_eq!(content, "first prompt");
    }

    #[test]
    fn context_md_contains_metadata_and_actions() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = SessionWriter::new(dir.path().to_str().unwrap(), "claude-opus-4-6");
        writer.dir = dir.path().join("session-context");

        // Simulate a tool_use action being recorded
        let msg = Message {
            role: "assistant".to_string(),
            content: vec![ContentBlock::ToolUse {
                id: "tu_1".to_string(),
                name: "Read".to_string(),
                input: serde_json::json!({"file_path": "/src/main.rs"}),
            }],
        };
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };
        writer.append_assistant_turn(&msg, &usage);

        writer.write_context();

        let ctx_path = writer.dir.join("context.md");
        let content = fs::read_to_string(ctx_path).unwrap();
        assert!(content.contains("Session ID:"));
        assert!(content.contains("claude-opus-4-6"));
        assert!(content.contains("Key Actions"));
        assert!(content.contains("**Read**: /src/main.rs"));
    }

    #[test]
    fn timestamp_is_iso8601() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = SessionWriter::new(dir.path().to_str().unwrap(), "test-model");
        writer.dir = dir.path().join("session-ts");

        let msg = Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text {
                text: "test".to_string(),
            }],
        };
        writer.append_user_turn(&msg);

        let jsonl_path = writer.dir.join("full.jsonl");
        let line = fs::read_to_string(&jsonl_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();

        let ts = parsed["timestamp"].as_str().unwrap();
        // Parse as RFC 3339 (superset of ISO 8601)
        assert!(
            chrono::DateTime::parse_from_rfc3339(ts).is_ok(),
            "timestamp should be valid ISO 8601: {ts}"
        );
    }

    #[test]
    fn extract_first_arg_from_object() {
        let input = serde_json::json!({"file_path": "/src/main.rs", "other": "val"});
        let result = extract_first_arg(&input);
        // serde_json objects iterate in insertion order for Map
        assert_eq!(result, "/src/main.rs");
    }

    #[test]
    fn extract_first_arg_truncates_long_values() {
        let long_val = "x".repeat(200);
        let input = serde_json::json!({"content": long_val});
        let result = extract_first_arg(&input);
        assert!(result.len() < 100);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn user_turn_has_no_usage() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = SessionWriter::new(dir.path().to_str().unwrap(), "test-model");
        writer.dir = dir.path().join("session-no-usage");

        let msg = Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text {
                text: "hello".to_string(),
            }],
        };
        writer.append_user_turn(&msg);

        let jsonl_path = writer.dir.join("full.jsonl");
        let line = fs::read_to_string(&jsonl_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();

        assert!(
            parsed["message"].get("usage").is_none(),
            "user turns should not have usage field"
        );
    }
}
