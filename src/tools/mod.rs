use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Generate `all_tool_schemas()` from a declarative tool list.
/// Each entry: name, description, schema (serde_json::Value).
macro_rules! tools {
    ( $( $name:expr, $desc:expr, $schema:expr );+ $(;)? ) => {
        pub fn all_tool_schemas() -> Vec<serde_json::Value> {
            vec![
                $(
                    serde_json::json!({
                        "name": $name,
                        "description": $desc,
                        "input_schema": $schema,
                    }),
                )+
            ]
        }
    };
}

tools! {
    "Read", "Read a file from disk. Returns file contents as text. Binary files return a placeholder message. Maximum 1MB file size.",
    json!({
        "type": "object",
        "properties": {
            "file_path": {
                "type": "string",
                "description": "Absolute or relative path to the file to read"
            }
        },
        "required": ["file_path"]
    });

    "Glob", "List files matching a glob pattern. Returns up to 1000 entries sorted by modification time.",
    json!({
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "Glob pattern (e.g. '**/*.rs', 'src/*.ts')"
            },
            "path": {
                "type": "string",
                "description": "Base directory to search from (default: current directory)"
            }
        },
        "required": ["pattern"]
    });

    "Bash", "Execute a bash command. Returns stdout and stderr. 120 second timeout. Streaming output.",
    json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "The bash command to execute"
            },
            "description": {
                "type": "string",
                "description": "Brief description of what the command does"
            }
        },
        "required": ["command"]
    });

    "Edit", "Edit a file by replacing exact text matches. Maximum 100KB file size. Default: single exact match. Set replace_all=true for bulk replacements. Empty old_str on missing file creates the file (with parent directories). Empty old_str on existing file appends content.",
    json!({
        "type": "object",
        "properties": {
            "file_path": {
                "type": "string",
                "description": "Path to the file to edit"
            },
            "old_str": {
                "type": "string",
                "description": "Exact text to find and replace (empty string = create/append)"
            },
            "new_str": {
                "type": "string",
                "description": "Replacement text"
            },
            "replace_all": {
                "type": "boolean",
                "description": "Replace all occurrences instead of requiring a single unique match (default: false)"
            }
        },
        "required": ["file_path", "old_str", "new_str"]
    });

    "Grep", "Search file contents using ripgrep (rg). Returns up to 50 matches. Requires rg to be installed.",
    json!({
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "Regex pattern to search for"
            },
            "path": {
                "type": "string",
                "description": "File or directory to search in (default: current directory)"
            },
            "file_type": {
                "type": "string",
                "description": "File type filter (e.g. 'rs', 'py', 'js')"
            },
            "case_sensitive": {
                "type": "boolean",
                "description": "Case-sensitive search (default: true)"
            }
        },
        "required": ["pattern"]
    });
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ToolEffect {
    Pure,
    Mutating,
}

pub fn tool_effect(name: &str) -> ToolEffect {
    match name {
        "Read" | "Glob" | "Grep" => ToolEffect::Pure,
        "Bash" | "Edit" => ToolEffect::Mutating,
        _ => ToolEffect::Mutating,
    }
}

/// Dispatch a tool call by name. Returns Ok(output) or Err(error_message).
/// Bash gets a streaming callback; other tools don't need one.
pub fn dispatch_tool(
    name: &str,
    input: &Value,
    stream_cb: &mut dyn FnMut(&str),
) -> Result<String, String> {
    match name {
        "Read" => read_exec(input),
        "Glob" => glob_exec(input),
        "Bash" => bash_exec(input, stream_cb),
        "Edit" => edit_exec(input),
        "Grep" => grep_exec(input),
        _ => Err(format!("Unknown tool: {name}")),
    }
}

fn read_exec(input: &Value) -> Result<String, String> {
    let file_path = input["file_path"]
        .as_str()
        .ok_or("Missing required parameter: file_path")?;

    let path = Path::new(file_path);
    if !path.exists() {
        return Err(format!("File not found: {file_path}"));
    }

    let metadata =
        std::fs::metadata(path).map_err(|e| format!("Cannot read file metadata: {e}"))?;
    if metadata.len() > 1_048_576 {
        return Err(format!(
            "File too large: {} bytes (limit: 1MB)",
            metadata.len()
        ));
    }

    let content = std::fs::read(path).map_err(|e| format!("Cannot read file: {e}"))?;

    // Check for binary content (NUL bytes in first 8KB)
    let check_len = content.len().min(8192);
    if content[..check_len].contains(&0) {
        return Ok(format!(
            "[Binary file: {file_path}, {} bytes]",
            content.len()
        ));
    }

    String::from_utf8(content).map_err(|_| format!("File contains invalid UTF-8: {file_path}"))
}

fn glob_exec(input: &Value) -> Result<String, String> {
    let pattern = input["pattern"]
        .as_str()
        .ok_or("Missing required parameter: pattern")?;
    let base = input["path"].as_str().unwrap_or(".");

    // Shell out to find with glob, or use a simpler approach
    // Using bash for glob expansion to avoid pulling in the glob crate
    let full_pattern = if pattern.starts_with('/') || pattern.starts_with('.') {
        pattern.to_string()
    } else {
        format!("{base}/{pattern}")
    };

    let output = Command::new("bash")
        .arg("-c")
        .arg(format!(
            "shopt -s globstar nullglob; files=({full_pattern}); printf '%s\\n' \"${{files[@]}}\" | head -1000"
        ))
        .output()
        .map_err(|e| format!("Failed to execute glob: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let result = stdout.trim().to_string();

    if result.is_empty() {
        Ok("No files found".to_string())
    } else {
        Ok(result)
    }
}

/// Deny-list patterns for bash commands. Whitespace-normalized lowercase matching.
const BASH_DENY_LIST: &[&str] = &[
    "rm -rf /",
    "rm -fr /",
    "rm -rf /*",
    "rm -fr /*",
    ":(){ :|:& };:",
    "dd if=/dev",
    "mkfs",
    "chmod 777 /",
    "git push --force",
    "git push -f",
];

fn normalize_command(cmd: &str) -> String {
    cmd.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_denied_command(cmd: &str) -> bool {
    let normalized = normalize_command(cmd);
    BASH_DENY_LIST
        .iter()
        .any(|pattern| normalized.contains(pattern))
}

fn bash_exec(input: &Value, stream_cb: &mut dyn FnMut(&str)) -> Result<String, String> {
    let command = input["command"]
        .as_str()
        .ok_or("Missing required parameter: command")?;

    if is_denied_command(command) {
        return Err(format!("Command blocked by safety guard: {command}"));
    }

    let timeout = Duration::from_secs(120);
    let deadline = Instant::now() + timeout;

    let mut child = Command::new("bash")
        .arg("-c")
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn bash: {e}"))?;

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let (tx_out, rx) = mpsc::channel::<String>();
    let tx_err = tx_out.clone();

    // Stdout reader thread
    std::thread::spawn(move || {
        let reader = BufReader::with_capacity(4096, stdout);
        let mut buf = Vec::new();
        let mut r = reader;
        loop {
            buf.clear();
            match r.read_until(b'\n', &mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    let s = String::from_utf8_lossy(&buf).to_string();
                    if tx_out.send(s).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Stderr reader thread
    std::thread::spawn(move || {
        let reader = BufReader::with_capacity(4096, stderr);
        let mut buf = Vec::new();
        let mut r = reader;
        loop {
            buf.clear();
            match r.read_until(b'\n', &mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    let s = String::from_utf8_lossy(&buf).to_string();
                    if tx_err.send(s).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Drop our copy of tx so rx closes when threads finish
    let mut output = String::new();
    let mut timed_out = false;

    loop {
        if Instant::now() >= deadline {
            let _ = child.kill();
            timed_out = true;
            break;
        }

        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(chunk) => {
                stream_cb(&chunk);
                output.push_str(&chunk);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Check if process has exited
                match child.try_wait() {
                    Ok(Some(_)) => {
                        // Process finished — drain remaining
                        while let Ok(chunk) = rx.try_recv() {
                            stream_cb(&chunk);
                            output.push_str(&chunk);
                        }
                        break;
                    }
                    Ok(None) => continue,
                    Err(_) => break,
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Both threads done
                let _ = child.wait();
                break;
            }
        }
    }

    if timed_out {
        if output.is_empty() {
            return Err(format!("Command timed out after 120s: {command}"));
        }
        return Err(format!(
            "Command timed out after 120s (partial output):\n{output}"
        ));
    }

    let status = child
        .wait()
        .map_err(|e| format!("Failed to wait for process: {e}"))?;

    if status.success() {
        Ok(output)
    } else {
        let code = status.code().unwrap_or(-1);
        if output.is_empty() {
            Err(format!("Command failed with exit code {code}"))
        } else {
            Err(format!("Command failed with exit code {code}:\n{output}"))
        }
    }
}

fn edit_exec(input: &Value) -> Result<String, String> {
    let file_path = input["file_path"]
        .as_str()
        .ok_or("Missing required parameter: file_path")?;
    let old_str = input["old_str"]
        .as_str()
        .ok_or("Missing required parameter: old_str")?;
    let new_str = input["new_str"]
        .as_str()
        .ok_or("Missing required parameter: new_str")?;
    let replace_all = input["replace_all"].as_bool().unwrap_or(false);

    let path = Path::new(file_path);

    // Empty old_str: create file (if missing) or append (if exists)
    if old_str.is_empty() {
        if path.exists() {
            // Append
            let mut content =
                std::fs::read_to_string(path).map_err(|e| format!("Cannot read file: {e}"))?;
            content.push_str(new_str);
            std::fs::write(path, &content).map_err(|e| format!("Cannot write file: {e}"))?;
            return Ok(format!("Appended to {file_path}"));
        } else {
            // Create with parent dirs
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("Cannot create directories: {e}"))?;
            }
            std::fs::write(path, new_str).map_err(|e| format!("Cannot create file: {e}"))?;
            return Ok(format!("Created {file_path}"));
        }
    }

    // Normal edit: read, find, replace
    let content = std::fs::read_to_string(path).map_err(|e| format!("Cannot read file: {e}"))?;

    let metadata = std::fs::metadata(path).map_err(|e| format!("Cannot read metadata: {e}"))?;
    if metadata.len() > 102_400 {
        return Err(format!(
            "File too large for edit: {} bytes (limit: 100KB)",
            metadata.len()
        ));
    }

    if replace_all {
        if !content.contains(old_str) {
            return Err(format!("Text not found in {file_path}"));
        }
        let new_content = content.replace(old_str, new_str);
        let count = content.matches(old_str).count();
        std::fs::write(path, &new_content).map_err(|e| format!("Cannot write file: {e}"))?;
        return Ok(format!("Replaced {count} occurrences in {file_path}"));
    }

    // Single exact match
    let count = content.matches(old_str).count();
    if count == 0 {
        return Err(format!("Text not found in {file_path}"));
    }
    if count > 1 {
        return Err(format!(
            "Found {count} matches in {file_path} (expected exactly 1). Use replace_all=true for bulk replacement."
        ));
    }

    let new_content = content.replacen(old_str, new_str, 1);
    std::fs::write(path, &new_content).map_err(|e| format!("Cannot write file: {e}"))?;

    Ok(format!("Edited {file_path}"))
}

fn grep_exec(input: &Value) -> Result<String, String> {
    let pattern = input["pattern"]
        .as_str()
        .ok_or("Missing required parameter: pattern")?;
    let path = input["path"].as_str().unwrap_or(".");
    let file_type = input["file_type"].as_str();
    let case_sensitive = input["case_sensitive"].as_bool().unwrap_or(true);

    // Check rg is installed
    let rg_check = Command::new("which").arg("rg").output();
    match rg_check {
        Ok(output) if !output.status.success() => {
            return Err(
                "ripgrep (rg) is not installed. Install it with: brew install ripgrep (macOS) or apt install ripgrep (Linux)"
                    .to_string(),
            );
        }
        Err(_) => {
            return Err(
                "ripgrep (rg) is not installed. Install it with: brew install ripgrep (macOS) or apt install ripgrep (Linux)"
                    .to_string(),
            );
        }
        _ => {}
    }

    let mut cmd = Command::new("rg");
    cmd.arg("--max-count=50")
        .arg("--line-number")
        .arg("--no-heading")
        .arg("--color=never");

    if !case_sensitive {
        cmd.arg("-i");
    }

    if let Some(ft) = file_type {
        cmd.arg("--type").arg(ft);
    }

    cmd.arg(pattern).arg(path);

    let output = cmd.output().map_err(|e| format!("Failed to run rg: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() {
        let code = output.status.code().unwrap_or(-1);
        if code == 1 {
            // rg exit 1 = no matches
            return Ok("No matches found".to_string());
        }
        if !stderr.is_empty() {
            return Err(stderr.trim().to_string());
        }
        return Err(format!("rg exited with code {code}"));
    }

    let result = stdout.trim().to_string();
    if result.is_empty() {
        Ok("No matches found".to_string())
    } else {
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_returns_five_pascal_case() {
        let schemas = all_tool_schemas();
        assert_eq!(schemas.len(), 5);

        let names: Vec<&str> = schemas
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["Read", "Glob", "Bash", "Edit", "Grep"]);
    }

    #[test]
    fn dispatch_known_tool_read() {
        // Read a file that definitely exists
        let result = dispatch_tool("Read", &json!({"file_path": "Cargo.toml"}), &mut |_| {});
        assert!(result.is_ok());
        assert!(result.unwrap().contains("[package]"));
    }

    #[test]
    fn dispatch_unknown_tool() {
        let result = dispatch_tool("Unknown", &json!({}), &mut |_| {});
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown tool"));
    }

    #[test]
    fn bash_deny_list_blocks_dangerous() {
        let cases = vec![
            "rm -rf /",
            "rm  -rf   /",
            "RM -RF /",
            "rm -fr /",
            "git push --force",
            "git push -f origin main",
            "git  push  --force",
        ];
        for cmd in cases {
            assert!(is_denied_command(cmd), "Expected deny for: {cmd}");
        }
    }

    #[test]
    fn bash_deny_list_allows_safe() {
        let cases = vec!["ls -la", "git push", "rm file.txt", "echo hello"];
        for cmd in cases {
            assert!(!is_denied_command(cmd), "Expected allow for: {cmd}");
        }
    }

    #[test]
    fn edit_replace_all_flag() {
        let dir = std::env::temp_dir().join("forgeflare_test_edit");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("test_replace_all.txt");
        std::fs::write(&file, "aaa bbb aaa ccc aaa").unwrap();

        let result = dispatch_tool(
            "Edit",
            &json!({
                "file_path": file.to_str().unwrap(),
                "old_str": "aaa",
                "new_str": "xxx",
                "replace_all": true,
            }),
            &mut |_| {},
        );
        assert!(result.is_ok());
        assert!(result.unwrap().contains("3 occurrences"));

        let content = std::fs::read_to_string(&file).unwrap();
        assert_eq!(content, "xxx bbb xxx ccc xxx");
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn edit_single_match_rejects_duplicates() {
        let dir = std::env::temp_dir().join("forgeflare_test_edit");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("test_dup.txt");
        std::fs::write(&file, "aaa bbb aaa").unwrap();

        let result = dispatch_tool(
            "Edit",
            &json!({
                "file_path": file.to_str().unwrap(),
                "old_str": "aaa",
                "new_str": "xxx",
            }),
            &mut |_| {},
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("2 matches"));
        assert!(err.contains("replace_all"));
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn edit_create_file() {
        let dir = std::env::temp_dir().join("forgeflare_test_edit");
        let file = dir.join("subdir/new_file.txt");
        let _ = std::fs::remove_file(&file);

        let result = dispatch_tool(
            "Edit",
            &json!({
                "file_path": file.to_str().unwrap(),
                "old_str": "",
                "new_str": "hello world",
            }),
            &mut |_| {},
        );
        assert!(result.is_ok());
        assert!(result.unwrap().contains("Created"));

        let content = std::fs::read_to_string(&file).unwrap();
        assert_eq!(content, "hello world");
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn edit_append_to_existing() {
        let dir = std::env::temp_dir().join("forgeflare_test_edit");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("test_append.txt");
        std::fs::write(&file, "existing").unwrap();

        let result = dispatch_tool(
            "Edit",
            &json!({
                "file_path": file.to_str().unwrap(),
                "old_str": "",
                "new_str": " appended",
            }),
            &mut |_| {},
        );
        assert!(result.is_ok());
        assert!(result.unwrap().contains("Appended"));

        let content = std::fs::read_to_string(&file).unwrap();
        assert_eq!(content, "existing appended");
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn read_missing_file() {
        let result = dispatch_tool(
            "Read",
            &json!({"file_path": "/nonexistent/file.txt"}),
            &mut |_| {},
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn bash_simple_command() {
        let mut streamed = String::new();
        let result = dispatch_tool("Bash", &json!({"command": "echo hello"}), &mut |text| {
            streamed.push_str(text);
        });
        assert!(result.is_ok());
        assert!(result.unwrap().trim() == "hello");
    }

    #[test]
    fn grep_no_matches() {
        // Use a temp dir with a known file to avoid matching source code
        let dir = std::env::temp_dir().join("forgeflare_test_grep");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("sample.txt");
        std::fs::write(&file, "some content here").unwrap();

        let result = dispatch_tool(
            "Grep",
            &json!({"pattern": "zzz_no_match_zzz", "path": dir.to_str().unwrap()}),
            &mut |_| {},
        );
        assert!(result.is_ok());
        assert!(result.unwrap().contains("No matches"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- ToolEffect classification tests ---

    #[test]
    fn tool_effect_pure_tools() {
        assert_eq!(tool_effect("Read"), ToolEffect::Pure);
        assert_eq!(tool_effect("Glob"), ToolEffect::Pure);
        assert_eq!(tool_effect("Grep"), ToolEffect::Pure);
    }

    #[test]
    fn tool_effect_mutating_tools() {
        assert_eq!(tool_effect("Bash"), ToolEffect::Mutating);
        assert_eq!(tool_effect("Edit"), ToolEffect::Mutating);
    }

    #[test]
    fn tool_effect_unknown_defaults_to_mutating() {
        assert_eq!(tool_effect("Unknown"), ToolEffect::Mutating);
        assert_eq!(tool_effect("FutureTool"), ToolEffect::Mutating);
        assert_eq!(tool_effect(""), ToolEffect::Mutating);
    }

    #[test]
    fn tool_effect_exhaustive_for_all_tools() {
        let schemas = all_tool_schemas();
        for schema in &schemas {
            let name = schema["name"].as_str().unwrap();
            let effect = tool_effect(name);
            // Every known tool must have an explicit classification (not fall through to unknown)
            match name {
                "Read" | "Glob" | "Grep" => {
                    assert_eq!(effect, ToolEffect::Pure, "{name} should be Pure")
                }
                "Bash" | "Edit" => {
                    assert_eq!(effect, ToolEffect::Mutating, "{name} should be Mutating")
                }
                _ => panic!("New tool {name} needs explicit ToolEffect classification"),
            }
        }
    }
}
