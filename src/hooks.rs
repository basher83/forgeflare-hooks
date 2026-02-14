use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

const DEFAULT_TIMEOUT_MS: u64 = 5000;
const DEFAULT_STOP_TIMEOUT_MS: u64 = 3000;
const RESULT_TRUNCATION_LIMIT: usize = 5120;
const RESULT_HALF: usize = 2560;

#[derive(Debug, Deserialize)]
struct HooksFile {
    hooks: Vec<HookConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HookConfig {
    pub event: String,
    pub command: String,
    pub match_tool: Option<String>,
    pub phase: Option<String>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, PartialEq)]
pub enum PreToolResult {
    Allow,
    Block { reason: String, blocked_by: String },
}

#[derive(Debug, PartialEq)]
pub enum PostToolResult {
    Continue,
    Signal { signal: String, reason: String },
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ConvergenceState {
    #[serde(default)]
    observations: Vec<Observation>,
    #[serde(rename = "final", skip_serializing_if = "Option::is_none")]
    final_state: Option<FinalState>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Observation {
    signal: String,
    reason: String,
    tool_iterations: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct FinalState {
    reason: String,
    tool_iterations: usize,
    total_tokens: u64,
    timestamp: String,
}

#[derive(Debug, Deserialize)]
struct GuardOutput {
    action: String,
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PostOutput {
    action: String,
    signal: Option<String>,
    reason: Option<String>,
}

pub struct HookRunner {
    hooks: Vec<HookConfig>,
    cwd: String,
    convergence_dir: PathBuf,
    convergence_path: PathBuf,
    convergence_tmp: PathBuf,
}

impl HookRunner {
    pub fn load(config_path: &str, cwd: &str) -> Self {
        let hooks = match fs::read_to_string(config_path) {
            Ok(content) => match toml::from_str::<HooksFile>(&content) {
                Ok(file) => file.hooks,
                Err(e) => {
                    eprintln!("[hooks] Failed to parse {config_path}: {e}");
                    Vec::new()
                }
            },
            Err(_) => Vec::new(),
        };

        let cwd_path = PathBuf::from(cwd);
        let convergence_dir = cwd_path.join(".forgeflare");
        let convergence_path = convergence_dir.join("convergence.json");
        let convergence_tmp = convergence_dir.join("convergence.json.tmp");

        Self {
            hooks,
            cwd: cwd.to_string(),
            convergence_dir,
            convergence_path,
            convergence_tmp,
        }
    }

    pub fn clear_convergence_state(&self) {
        match fs::remove_file(&self.convergence_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                eprintln!(
                    "[hooks] Warning: failed to remove {}: {e}",
                    self.convergence_path.display()
                );
            }
        }
    }

    pub async fn run_pre_tool_use(
        &self,
        tool: &str,
        input: &Value,
        tool_iterations: usize,
    ) -> PreToolResult {
        // Guard phase
        let guard_hooks: Vec<&HookConfig> = self
            .hooks
            .iter()
            .filter(|h| h.event == "PreToolUse")
            .filter(|h| {
                let phase = h.phase.as_deref().unwrap_or("guard");
                phase == "guard"
            })
            .filter(|h| matches_tool(h, tool))
            .collect();

        let mut blocked = false;
        let mut blocked_by = String::new();
        let mut block_reason = String::new();

        for hook in &guard_hooks {
            let hook_input = serde_json::json!({
                "event": "PreToolUse",
                "phase": "guard",
                "tool": tool,
                "input": input,
                "tool_iterations": tool_iterations,
                "cwd": self.cwd,
            });

            let timeout = hook.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);

            match run_hook_subprocess(&hook.command, &hook_input, timeout).await {
                Ok(stdout) => match serde_json::from_str::<GuardOutput>(&stdout) {
                    Ok(output) => {
                        if output.action == "block" {
                            blocked = true;
                            blocked_by = hook.command.clone();
                            block_reason = output
                                .reason
                                .unwrap_or_else(|| "no reason provided".to_string());
                            break;
                        }
                        // "allow" or anything else: continue to next guard
                    }
                    Err(_) => {
                        blocked = true;
                        blocked_by = hook.command.clone();
                        block_reason = format!(
                            "hook failed: {} returned invalid JSON (tool blocked by default)",
                            hook.command
                        );
                        break;
                    }
                },
                Err(HookError::Timeout(ms)) => {
                    blocked = true;
                    blocked_by = hook.command.clone();
                    block_reason = format!(
                        "hook failed: {} timed out after {ms}ms (tool blocked by default)",
                        hook.command
                    );
                    break;
                }
                Err(HookError::NonZeroExit(code)) => {
                    blocked = true;
                    blocked_by = hook.command.clone();
                    block_reason = format!(
                        "hook failed: {} exited with code {code} (tool blocked by default)",
                        hook.command
                    );
                    break;
                }
                Err(HookError::Spawn(msg)) => {
                    blocked = true;
                    blocked_by = hook.command.clone();
                    block_reason = format!(
                        "hook failed: {} spawn error: {msg} (tool blocked by default)",
                        hook.command
                    );
                    break;
                }
            }
        }

        // Observe phase — always runs, with guard outcome context
        let observe_hooks: Vec<&HookConfig> = self
            .hooks
            .iter()
            .filter(|h| h.event == "PreToolUse")
            .filter(|h| h.phase.as_deref() == Some("observe"))
            .filter(|h| matches_tool(h, tool))
            .collect();

        for hook in &observe_hooks {
            let mut hook_input = serde_json::json!({
                "event": "PreToolUse",
                "phase": "observe",
                "tool": tool,
                "input": input,
                "blocked": blocked,
                "tool_iterations": tool_iterations,
                "cwd": self.cwd,
            });

            if blocked {
                hook_input["blocked_by"] = Value::String(blocked_by.clone());
                hook_input["block_reason"] = Value::String(block_reason.clone());
            }

            let timeout = hook.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);

            // Fail-open: errors logged but don't affect outcome
            match run_hook_subprocess(&hook.command, &hook_input, timeout).await {
                Ok(_) => {} // Output ignored for observe hooks
                Err(e) => {
                    eprintln!("[hooks] Observe hook {} failed: {e}", hook.command);
                }
            }
        }

        if blocked {
            let reason = if block_reason.starts_with("hook failed:") {
                block_reason
            } else {
                format!("blocked by {blocked_by}: {block_reason}")
            };
            PreToolResult::Block { reason, blocked_by }
        } else {
            PreToolResult::Allow
        }
    }

    pub async fn run_post_tool_use(
        &self,
        tool: &str,
        input: &Value,
        result: &str,
        is_error: bool,
        tool_iterations: usize,
    ) -> PostToolResult {
        let matching_hooks: Vec<&HookConfig> = self
            .hooks
            .iter()
            .filter(|h| h.event == "PostToolUse")
            .filter(|h| matches_tool(h, tool))
            .collect();

        if matching_hooks.is_empty() {
            return PostToolResult::Continue;
        }

        let truncated_result = truncate_result(result);

        let mut first_signal: Option<PostToolResult> = None;
        let mut observations: Vec<Observation> = Vec::new();

        for hook in &matching_hooks {
            let hook_input = serde_json::json!({
                "event": "PostToolUse",
                "tool": tool,
                "input": input,
                "result": truncated_result,
                "is_error": is_error,
                "tool_iterations": tool_iterations,
                "cwd": self.cwd,
            });

            let timeout = hook.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);

            // Fail-open
            match run_hook_subprocess(&hook.command, &hook_input, timeout).await {
                Ok(stdout) => match serde_json::from_str::<PostOutput>(&stdout) {
                    Ok(output) => {
                        if output.action == "signal" {
                            let signal = output.signal.unwrap_or_else(|| "unknown".to_string());
                            let reason = output.reason.unwrap_or_else(|| "no reason".to_string());

                            observations.push(Observation {
                                signal: signal.clone(),
                                reason: reason.clone(),
                                tool_iterations,
                            });

                            if first_signal.is_none() {
                                first_signal = Some(PostToolResult::Signal { signal, reason });
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "[hooks] PostToolUse hook {} returned invalid JSON: {e}",
                            hook.command
                        );
                    }
                },
                Err(e) => {
                    eprintln!("[hooks] PostToolUse hook {} failed: {e}", hook.command);
                }
            }
        }

        // Single read-modify-write for all observations
        if !observations.is_empty() {
            if let Err(e) = write_observations(
                &observations,
                &self.convergence_dir,
                &self.convergence_path,
                &self.convergence_tmp,
            ) {
                eprintln!("[hooks] Warning: failed to write convergence observations: {e}");
            }
        }

        first_signal.unwrap_or(PostToolResult::Continue)
    }

    pub async fn run_stop(&self, reason: &str, tool_iterations: usize, total_tokens: u64) {
        let matching_hooks: Vec<&HookConfig> =
            self.hooks.iter().filter(|h| h.event == "Stop").collect();

        for hook in &matching_hooks {
            let hook_input = serde_json::json!({
                "event": "Stop",
                "reason": reason,
                "tool_iterations": tool_iterations,
                "total_tokens": total_tokens,
                "cwd": self.cwd,
            });

            let timeout = hook.timeout_ms.unwrap_or(DEFAULT_STOP_TIMEOUT_MS);

            // Fail-open
            match run_hook_subprocess(&hook.command, &hook_input, timeout).await {
                Ok(stdout) => {
                    // Parse for logging only
                    if let Ok(parsed) = serde_json::from_str::<Value>(&stdout) {
                        let action = parsed["action"].as_str().unwrap_or("unknown");
                        if action != "continue" {
                            eprintln!(
                                "[hooks] Stop hook {} returned unrecognized action: {action}",
                                hook.command
                            );
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[hooks] Stop hook {} failed: {e}", hook.command);
                }
            }
        }

        // Write final state to convergence.json
        if let Err(e) = write_final_state(
            reason,
            tool_iterations,
            total_tokens,
            &self.convergence_dir,
            &self.convergence_path,
            &self.convergence_tmp,
        ) {
            eprintln!("[hooks] Warning: failed to write convergence final state: {e}");
        }
    }

    pub fn has_hooks(&self) -> bool {
        !self.hooks.is_empty()
    }
}

fn matches_tool(hook: &HookConfig, tool: &str) -> bool {
    match &hook.match_tool {
        Some(mt) => mt == tool,
        None => true,
    }
}

fn truncate_result(result: &str) -> String {
    if result.len() <= RESULT_TRUNCATION_LIMIT {
        return result.to_string();
    }

    let first_end = result.floor_char_boundary(RESULT_HALF);
    let last_start = result.floor_char_boundary(result.len() - RESULT_HALF);

    format!(
        "{}\n... (truncated for hook, full result: {} bytes)\n{}",
        &result[..first_end],
        result.len(),
        &result[last_start..],
    )
}

#[derive(Debug)]
enum HookError {
    Timeout(u64),
    NonZeroExit(i32),
    Spawn(String),
}

impl std::fmt::Display for HookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HookError::Timeout(ms) => write!(f, "timed out after {ms}ms"),
            HookError::NonZeroExit(code) => write!(f, "exited with code {code}"),
            HookError::Spawn(msg) => write!(f, "spawn error: {msg}"),
        }
    }
}

async fn run_hook_subprocess(
    command: &str,
    input: &Value,
    timeout_ms: u64,
) -> Result<String, HookError> {
    let stdin_data = serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string());

    let result = tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), async {
        let mut child = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(command)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .map_err(|e| HookError::Spawn(e.to_string()))?;

        // Write stdin
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(stdin_data.as_bytes())
                .await
                .map_err(|e| HookError::Spawn(format!("stdin write: {e}")))?;
            // Drop stdin to close it
        }

        let output = child
            .wait_with_output()
            .await
            .map_err(|e| HookError::Spawn(e.to_string()))?;

        if !output.status.success() {
            let code = output.status.code().unwrap_or(-1);
            return Err(HookError::NonZeroExit(code));
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    })
    .await;

    match result {
        Ok(inner) => inner,
        Err(_) => Err(HookError::Timeout(timeout_ms)),
    }
}

fn write_observations(
    new_observations: &[Observation],
    dir: &Path,
    path: &Path,
    tmp: &Path,
) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;

    let mut state = match fs::read_to_string(path) {
        Ok(content) => serde_json::from_str::<ConvergenceState>(&content).unwrap_or_default(),
        Err(_) => ConvergenceState::default(),
    };

    for obs in new_observations {
        state.observations.push(Observation {
            signal: obs.signal.clone(),
            reason: obs.reason.clone(),
            tool_iterations: obs.tool_iterations,
        });
    }

    let json = serde_json::to_string_pretty(&state).map_err(std::io::Error::other)?;
    fs::write(tmp, &json)?;
    fs::rename(tmp, path)?;

    Ok(())
}

fn write_final_state(
    reason: &str,
    tool_iterations: usize,
    total_tokens: u64,
    dir: &Path,
    path: &Path,
    tmp: &Path,
) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;

    let mut state = match fs::read_to_string(path) {
        Ok(content) => serde_json::from_str::<ConvergenceState>(&content).unwrap_or_default(),
        Err(_) => ConvergenceState::default(),
    };

    state.final_state = Some(FinalState {
        reason: reason.to_string(),
        tool_iterations,
        total_tokens,
        timestamp: Utc::now().to_rfc3339(),
    });

    let json = serde_json::to_string_pretty(&state).map_err(std::io::Error::other)?;
    fs::write(tmp, &json)?;
    fs::rename(tmp, path)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_file_returns_empty_runner() {
        let runner = HookRunner::load("/nonexistent/hooks.toml", "/tmp");
        assert!(!runner.has_hooks());
    }

    #[test]
    fn load_valid_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("hooks.toml");
        fs::write(
            &config_path,
            r#"
[[hooks]]
event = "PreToolUse"
command = "echo allow"
match_tool = "Bash"

[[hooks]]
event = "PostToolUse"
command = "echo continue"
timeout_ms = 3000
"#,
        )
        .unwrap();

        let runner = HookRunner::load(config_path.to_str().unwrap(), "/tmp");
        assert!(runner.has_hooks());
        assert_eq!(runner.hooks.len(), 2);
        assert_eq!(runner.hooks[0].event, "PreToolUse");
        assert_eq!(runner.hooks[0].match_tool, Some("Bash".to_string()));
        assert!(runner.hooks[0].phase.is_none());
        assert_eq!(runner.hooks[1].timeout_ms, Some(3000));
    }

    #[test]
    fn matches_tool_exact() {
        let hook = HookConfig {
            event: "PreToolUse".to_string(),
            command: "test".to_string(),
            match_tool: Some("Bash".to_string()),
            phase: None,
            timeout_ms: None,
        };
        assert!(matches_tool(&hook, "Bash"));
        assert!(!matches_tool(&hook, "Read"));
        assert!(!matches_tool(&hook, "BashScript")); // no prefix match
    }

    #[test]
    fn matches_tool_none_matches_all() {
        let hook = HookConfig {
            event: "PreToolUse".to_string(),
            command: "test".to_string(),
            match_tool: None,
            phase: None,
            timeout_ms: None,
        };
        assert!(matches_tool(&hook, "Bash"));
        assert!(matches_tool(&hook, "Read"));
        assert!(matches_tool(&hook, "Edit"));
    }

    #[test]
    fn truncate_result_under_limit() {
        let short = "hello world";
        assert_eq!(truncate_result(short), short);
    }

    #[test]
    fn truncate_result_at_limit() {
        let exact = "x".repeat(RESULT_TRUNCATION_LIMIT);
        assert_eq!(truncate_result(&exact), exact);
    }

    #[test]
    fn truncate_result_over_limit() {
        let long = "x".repeat(10000);
        let truncated = truncate_result(&long);
        assert!(truncated.len() < long.len());
        assert!(truncated.contains("truncated for hook, full result: 10000 bytes"));
        // First and last parts preserved
        assert!(truncated.starts_with(&"x".repeat(RESULT_HALF)));
        assert!(truncated.ends_with(&"x".repeat(RESULT_HALF)));
    }

    #[tokio::test]
    async fn guard_block_produces_error_reason() {
        let dir = tempfile::tempdir().unwrap();
        let hook_script = dir.path().join("guard.sh");
        fs::write(
            &hook_script,
            r#"#!/bin/bash
echo '{"action":"block","reason":"destructive command detected"}'
"#,
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook_script, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let config_path = dir.path().join("hooks.toml");
        fs::write(
            &config_path,
            format!(
                r#"[[hooks]]
event = "PreToolUse"
phase = "guard"
command = "{}"
match_tool = "Bash"
timeout_ms = 5000
"#,
                hook_script.display()
            ),
        )
        .unwrap();

        let runner = HookRunner::load(config_path.to_str().unwrap(), dir.path().to_str().unwrap());
        let result = runner
            .run_pre_tool_use("Bash", &serde_json::json!({"command": "rm -rf /"}), 0)
            .await;

        match result {
            PreToolResult::Block { reason, blocked_by } => {
                assert!(reason.contains("destructive command detected"));
                assert!(blocked_by.contains("guard.sh"));
            }
            PreToolResult::Allow => panic!("expected block"),
        }
    }

    #[tokio::test]
    async fn guard_allow_passes_through() {
        let dir = tempfile::tempdir().unwrap();
        let hook_script = dir.path().join("allow.sh");
        fs::write(&hook_script, "#!/bin/bash\necho '{\"action\":\"allow\"}'\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook_script, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let config_path = dir.path().join("hooks.toml");
        fs::write(
            &config_path,
            format!(
                "[[hooks]]\nevent = \"PreToolUse\"\nphase = \"guard\"\ncommand = \"{}\"\nmatch_tool = \"Bash\"\n",
                hook_script.display()
            ),
        )
        .unwrap();

        let runner = HookRunner::load(config_path.to_str().unwrap(), dir.path().to_str().unwrap());
        let result = runner
            .run_pre_tool_use("Bash", &serde_json::json!({"command": "ls"}), 0)
            .await;

        assert_eq!(result, PreToolResult::Allow);
    }

    #[tokio::test]
    async fn guard_timeout_blocks_tool() {
        let dir = tempfile::tempdir().unwrap();
        let hook_script = dir.path().join("slow.sh");
        fs::write(&hook_script, "#!/bin/bash\nsleep 10\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook_script, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let config_path = dir.path().join("hooks.toml");
        fs::write(
            &config_path,
            format!(
                "[[hooks]]\nevent = \"PreToolUse\"\nphase = \"guard\"\ncommand = \"{}\"\nmatch_tool = \"Bash\"\ntimeout_ms = 100\n",
                hook_script.display()
            ),
        )
        .unwrap();

        let runner = HookRunner::load(config_path.to_str().unwrap(), dir.path().to_str().unwrap());
        let result = runner
            .run_pre_tool_use("Bash", &serde_json::json!({"command": "ls"}), 0)
            .await;

        match result {
            PreToolResult::Block { reason, .. } => {
                assert!(reason.contains("timed out after 100ms"));
            }
            PreToolResult::Allow => panic!("expected block on timeout"),
        }
    }

    #[tokio::test]
    async fn guard_crash_blocks_tool() {
        let dir = tempfile::tempdir().unwrap();
        let hook_script = dir.path().join("crash.sh");
        fs::write(&hook_script, "#!/bin/bash\nexit 42\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook_script, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let config_path = dir.path().join("hooks.toml");
        fs::write(
            &config_path,
            format!(
                "[[hooks]]\nevent = \"PreToolUse\"\nphase = \"guard\"\ncommand = \"{}\"\n",
                hook_script.display()
            ),
        )
        .unwrap();

        let runner = HookRunner::load(config_path.to_str().unwrap(), dir.path().to_str().unwrap());
        let result = runner
            .run_pre_tool_use("Bash", &serde_json::json!({"command": "ls"}), 0)
            .await;

        match result {
            PreToolResult::Block { reason, .. } => {
                assert!(reason.contains("exited with code 42"));
            }
            PreToolResult::Allow => panic!("expected block on crash"),
        }
    }

    #[tokio::test]
    async fn guard_invalid_json_blocks_tool() {
        let dir = tempfile::tempdir().unwrap();
        let hook_script = dir.path().join("bad_json.sh");
        fs::write(&hook_script, "#!/bin/bash\necho 'not json'\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook_script, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let config_path = dir.path().join("hooks.toml");
        fs::write(
            &config_path,
            format!(
                "[[hooks]]\nevent = \"PreToolUse\"\nphase = \"guard\"\ncommand = \"{}\"\n",
                hook_script.display()
            ),
        )
        .unwrap();

        let runner = HookRunner::load(config_path.to_str().unwrap(), dir.path().to_str().unwrap());
        let result = runner
            .run_pre_tool_use("Bash", &serde_json::json!({"command": "ls"}), 0)
            .await;

        match result {
            PreToolResult::Block { reason, .. } => {
                assert!(reason.contains("invalid JSON"));
            }
            PreToolResult::Allow => panic!("expected block on invalid JSON"),
        }
    }

    #[tokio::test]
    async fn observe_runs_after_block() {
        let dir = tempfile::tempdir().unwrap();

        let guard_script = dir.path().join("guard.sh");
        fs::write(
            &guard_script,
            "#!/bin/bash\necho '{\"action\":\"block\",\"reason\":\"nope\"}'\n",
        )
        .unwrap();

        let observe_log = dir.path().join("observe.log");
        let observe_script = dir.path().join("observe.sh");
        fs::write(
            &observe_script,
            format!(
                "#!/bin/bash\ncat > {}\necho '{{}}'\n",
                observe_log.display()
            ),
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&guard_script, fs::Permissions::from_mode(0o755)).unwrap();
            fs::set_permissions(&observe_script, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let config_path = dir.path().join("hooks.toml");
        fs::write(
            &config_path,
            format!(
                "[[hooks]]\nevent = \"PreToolUse\"\nphase = \"guard\"\ncommand = \"{}\"\n\n\
                 [[hooks]]\nevent = \"PreToolUse\"\nphase = \"observe\"\ncommand = \"{}\"\n",
                guard_script.display(),
                observe_script.display()
            ),
        )
        .unwrap();

        let runner = HookRunner::load(config_path.to_str().unwrap(), dir.path().to_str().unwrap());
        let result = runner
            .run_pre_tool_use("Bash", &serde_json::json!({"command": "rm -rf /"}), 3)
            .await;

        assert!(matches!(result, PreToolResult::Block { .. }));

        // Observe hook should have been called with blocked context
        assert!(observe_log.exists(), "observe hook should have been called");
        let logged = fs::read_to_string(&observe_log).unwrap();
        let parsed: Value = serde_json::from_str(&logged).unwrap();
        assert_eq!(parsed["blocked"], true);
        assert_eq!(
            parsed["blocked_by"].as_str().unwrap(),
            guard_script.to_str().unwrap()
        );
        assert_eq!(parsed["block_reason"].as_str().unwrap(), "nope");
    }

    #[tokio::test]
    async fn observe_runs_after_allow() {
        let dir = tempfile::tempdir().unwrap();

        let guard_script = dir.path().join("guard.sh");
        fs::write(
            &guard_script,
            "#!/bin/bash\necho '{\"action\":\"allow\"}'\n",
        )
        .unwrap();

        let observe_log = dir.path().join("observe.log");
        let observe_script = dir.path().join("observe.sh");
        fs::write(
            &observe_script,
            format!(
                "#!/bin/bash\ncat > {}\necho '{{}}'\n",
                observe_log.display()
            ),
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&guard_script, fs::Permissions::from_mode(0o755)).unwrap();
            fs::set_permissions(&observe_script, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let config_path = dir.path().join("hooks.toml");
        fs::write(
            &config_path,
            format!(
                "[[hooks]]\nevent = \"PreToolUse\"\nphase = \"guard\"\ncommand = \"{}\"\n\n\
                 [[hooks]]\nevent = \"PreToolUse\"\nphase = \"observe\"\ncommand = \"{}\"\n",
                guard_script.display(),
                observe_script.display()
            ),
        )
        .unwrap();

        let runner = HookRunner::load(config_path.to_str().unwrap(), dir.path().to_str().unwrap());
        let result = runner
            .run_pre_tool_use("Bash", &serde_json::json!({"command": "ls"}), 5)
            .await;

        assert_eq!(result, PreToolResult::Allow);

        let logged = fs::read_to_string(&observe_log).unwrap();
        let parsed: Value = serde_json::from_str(&logged).unwrap();
        assert_eq!(parsed["blocked"], false);
        assert!(parsed.get("blocked_by").is_none());
    }

    #[tokio::test]
    async fn observe_failure_does_not_affect_outcome() {
        let dir = tempfile::tempdir().unwrap();

        let observe_script = dir.path().join("fail_observe.sh");
        fs::write(&observe_script, "#!/bin/bash\nexit 1\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&observe_script, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let config_path = dir.path().join("hooks.toml");
        fs::write(
            &config_path,
            format!(
                "[[hooks]]\nevent = \"PreToolUse\"\nphase = \"observe\"\ncommand = \"{}\"\n",
                observe_script.display()
            ),
        )
        .unwrap();

        let runner = HookRunner::load(config_path.to_str().unwrap(), dir.path().to_str().unwrap());
        // No guard hooks → Allow; observe failure should not change this
        let result = runner
            .run_pre_tool_use("Bash", &serde_json::json!({"command": "ls"}), 0)
            .await;

        assert_eq!(result, PreToolResult::Allow);
    }

    #[tokio::test]
    async fn post_tool_use_signal() {
        let dir = tempfile::tempdir().unwrap();
        let hook_script = dir.path().join("signal.sh");
        fs::write(
            &hook_script,
            "#!/bin/bash\necho '{\"action\":\"signal\",\"signal\":\"converged\",\"reason\":\"3 clean runs\"}'\n",
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook_script, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let config_path = dir.path().join("hooks.toml");
        fs::write(
            &config_path,
            format!(
                "[[hooks]]\nevent = \"PostToolUse\"\ncommand = \"{}\"\n",
                hook_script.display()
            ),
        )
        .unwrap();

        let runner = HookRunner::load(config_path.to_str().unwrap(), dir.path().to_str().unwrap());
        let result = runner
            .run_post_tool_use(
                "Bash",
                &serde_json::json!({"command": "cargo test"}),
                "ok",
                false,
                5,
            )
            .await;

        match result {
            PostToolResult::Signal { signal, reason } => {
                assert_eq!(signal, "converged");
                assert_eq!(reason, "3 clean runs");
            }
            PostToolResult::Continue => panic!("expected signal"),
        }

        // Check convergence file (uses absolute path via HookRunner)
        let conv_path = dir.path().join(".forgeflare/convergence.json");
        let conv = fs::read_to_string(&conv_path).unwrap();
        let state: ConvergenceState = serde_json::from_str(&conv).unwrap();
        assert_eq!(state.observations.len(), 1);
        assert_eq!(state.observations[0].signal, "converged");
        assert_eq!(state.observations[0].tool_iterations, 5);
    }

    #[tokio::test]
    async fn post_tool_use_continue() {
        let dir = tempfile::tempdir().unwrap();
        let hook_script = dir.path().join("continue.sh");
        fs::write(
            &hook_script,
            "#!/bin/bash\necho '{\"action\":\"continue\"}'\n",
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook_script, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let config_path = dir.path().join("hooks.toml");
        fs::write(
            &config_path,
            format!(
                "[[hooks]]\nevent = \"PostToolUse\"\ncommand = \"{}\"\n",
                hook_script.display()
            ),
        )
        .unwrap();

        let runner = HookRunner::load(config_path.to_str().unwrap(), dir.path().to_str().unwrap());
        let result = runner
            .run_post_tool_use("Bash", &serde_json::json!({}), "ok", false, 0)
            .await;

        assert_eq!(result, PostToolResult::Continue);
    }

    #[tokio::test]
    async fn post_tool_use_failure_returns_continue() {
        let dir = tempfile::tempdir().unwrap();
        let hook_script = dir.path().join("fail.sh");
        fs::write(&hook_script, "#!/bin/bash\nexit 1\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook_script, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let config_path = dir.path().join("hooks.toml");
        fs::write(
            &config_path,
            format!(
                "[[hooks]]\nevent = \"PostToolUse\"\ncommand = \"{}\"\n",
                hook_script.display()
            ),
        )
        .unwrap();

        let runner = HookRunner::load(config_path.to_str().unwrap(), dir.path().to_str().unwrap());
        let result = runner
            .run_post_tool_use("Bash", &serde_json::json!({}), "ok", false, 0)
            .await;

        assert_eq!(result, PostToolResult::Continue);
    }

    #[tokio::test]
    async fn stop_hook_fires() {
        let dir = tempfile::tempdir().unwrap();
        let stop_log = dir.path().join("stop.log");
        let hook_script = dir.path().join("stop.sh");
        fs::write(
            &hook_script,
            format!(
                "#!/bin/bash\ncat > {}\necho '{{\"action\":\"continue\"}}'\n",
                stop_log.display()
            ),
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook_script, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let config_path = dir.path().join("hooks.toml");
        fs::write(
            &config_path,
            format!(
                "[[hooks]]\nevent = \"Stop\"\ncommand = \"{}\"\n",
                hook_script.display()
            ),
        )
        .unwrap();

        let runner = HookRunner::load(config_path.to_str().unwrap(), dir.path().to_str().unwrap());
        runner.run_stop("end_turn", 7, 45000).await;

        // Check hook received correct input
        let logged = fs::read_to_string(&stop_log).unwrap();
        let parsed: Value = serde_json::from_str(&logged).unwrap();
        assert_eq!(parsed["event"], "Stop");
        assert_eq!(parsed["reason"], "end_turn");
        assert_eq!(parsed["tool_iterations"], 7);
        assert_eq!(parsed["total_tokens"], 45000);

        // Check convergence final state (absolute path)
        let conv_path = dir.path().join(".forgeflare/convergence.json");
        let conv = fs::read_to_string(&conv_path).unwrap();
        let state: ConvergenceState = serde_json::from_str(&conv).unwrap();
        assert!(state.final_state.is_some());
        let final_state = state.final_state.unwrap();
        assert_eq!(final_state.reason, "end_turn");
        assert_eq!(final_state.tool_iterations, 7);
        assert_eq!(final_state.total_tokens, 45000);
    }

    #[tokio::test]
    async fn stop_failure_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let hook_script = dir.path().join("fail_stop.sh");
        fs::write(&hook_script, "#!/bin/bash\nexit 1\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook_script, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let config_path = dir.path().join("hooks.toml");
        fs::write(
            &config_path,
            format!(
                "[[hooks]]\nevent = \"Stop\"\ncommand = \"{}\"\n",
                hook_script.display()
            ),
        )
        .unwrap();

        let runner = HookRunner::load(config_path.to_str().unwrap(), dir.path().to_str().unwrap());
        // Should not panic
        runner.run_stop("api_error", 3, 10000).await;
    }

    #[tokio::test]
    async fn no_matching_hooks_returns_allow() {
        let dir = tempfile::tempdir().unwrap();
        let hook_script = dir.path().join("guard.sh");
        fs::write(
            &hook_script,
            "#!/bin/bash\necho '{\"action\":\"block\",\"reason\":\"blocked\"}'\n",
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook_script, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let config_path = dir.path().join("hooks.toml");
        fs::write(
            &config_path,
            format!(
                "[[hooks]]\nevent = \"PreToolUse\"\nphase = \"guard\"\ncommand = \"{}\"\nmatch_tool = \"Bash\"\n",
                hook_script.display()
            ),
        )
        .unwrap();

        let runner = HookRunner::load(config_path.to_str().unwrap(), dir.path().to_str().unwrap());
        // Tool is "Read", hook only matches "Bash"
        let result = runner
            .run_pre_tool_use("Read", &serde_json::json!({"file_path": "test.txt"}), 0)
            .await;

        assert_eq!(result, PreToolResult::Allow);
    }

    #[tokio::test]
    async fn no_hooks_is_noop() {
        let runner = HookRunner::load("/nonexistent/hooks.toml", "/tmp");
        let pre = runner
            .run_pre_tool_use("Bash", &serde_json::json!({}), 0)
            .await;
        assert_eq!(pre, PreToolResult::Allow);

        let post = runner
            .run_post_tool_use("Bash", &serde_json::json!({}), "ok", false, 0)
            .await;
        assert_eq!(post, PostToolResult::Continue);

        // Stop should also be a no-op (doesn't panic)
        runner.run_stop("end_turn", 0, 0).await;
    }

    #[test]
    fn clear_convergence_state_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let ff_dir = dir.path().join(".forgeflare");
        fs::create_dir_all(&ff_dir).unwrap();
        let conv_file = ff_dir.join("convergence.json");
        fs::write(&conv_file, "{}").unwrap();
        assert!(conv_file.exists());

        let runner = HookRunner::load("/nonexistent", dir.path().to_str().unwrap());
        runner.clear_convergence_state();

        assert!(!conv_file.exists());
    }

    #[test]
    fn clear_convergence_state_missing_file_ok() {
        let dir = tempfile::tempdir().unwrap();
        let runner = HookRunner::load("/nonexistent", dir.path().to_str().unwrap());
        // Should not panic even though .forgeflare/ doesn't exist
        runner.clear_convergence_state();
    }

    #[test]
    fn convergence_atomic_write() {
        let dir = tempfile::tempdir().unwrap();
        let ff_dir = dir.path().join(".forgeflare");
        let conv_path = ff_dir.join("convergence.json");
        let conv_tmp = ff_dir.join("convergence.json.tmp");

        let observations = vec![Observation {
            signal: "test".to_string(),
            reason: "test reason".to_string(),
            tool_iterations: 5,
        }];

        write_observations(&observations, &ff_dir, &conv_path, &conv_tmp).unwrap();

        assert!(conv_path.exists());
        assert!(!conv_tmp.exists());

        let content = fs::read_to_string(&conv_path).unwrap();
        let state: ConvergenceState = serde_json::from_str(&content).unwrap();
        assert_eq!(state.observations.len(), 1);
        assert_eq!(state.observations[0].signal, "test");
    }

    #[test]
    fn convergence_observations_append() {
        let dir = tempfile::tempdir().unwrap();
        let ff_dir = dir.path().join(".forgeflare");
        let conv_path = ff_dir.join("convergence.json");
        let conv_tmp = ff_dir.join("convergence.json.tmp");

        let obs1 = vec![Observation {
            signal: "first".to_string(),
            reason: "reason1".to_string(),
            tool_iterations: 1,
        }];
        write_observations(&obs1, &ff_dir, &conv_path, &conv_tmp).unwrap();

        let obs2 = vec![Observation {
            signal: "second".to_string(),
            reason: "reason2".to_string(),
            tool_iterations: 2,
        }];
        write_observations(&obs2, &ff_dir, &conv_path, &conv_tmp).unwrap();

        let content = fs::read_to_string(&conv_path).unwrap();
        let state: ConvergenceState = serde_json::from_str(&content).unwrap();
        assert_eq!(state.observations.len(), 2);
        assert_eq!(state.observations[0].signal, "first");
        assert_eq!(state.observations[1].signal, "second");
    }

    #[test]
    fn convergence_final_state_written() {
        let dir = tempfile::tempdir().unwrap();
        let ff_dir = dir.path().join(".forgeflare");
        let conv_path = ff_dir.join("convergence.json");
        let conv_tmp = ff_dir.join("convergence.json.tmp");

        write_final_state(
            "convergence_signal",
            22,
            45000,
            &ff_dir,
            &conv_path,
            &conv_tmp,
        )
        .unwrap();

        let content = fs::read_to_string(&conv_path).unwrap();

        // Verify raw JSON uses "final" key (not "final_state") per spec
        assert!(
            content.contains("\"final\""),
            "convergence JSON must use \"final\" key per hooks.md spec"
        );
        assert!(
            !content.contains("\"final_state\""),
            "convergence JSON must not use \"final_state\" key"
        );

        let state: ConvergenceState = serde_json::from_str(&content).unwrap();
        assert!(state.final_state.is_some());
        let final_s = state.final_state.unwrap();
        assert_eq!(final_s.reason, "convergence_signal");
        assert_eq!(final_s.tool_iterations, 22);
        assert_eq!(final_s.total_tokens, 45000);
        assert!(!final_s.timestamp.is_empty());
    }

    #[tokio::test]
    async fn multiple_post_hooks_first_signal_wins() {
        let dir = tempfile::tempdir().unwrap();

        let hook1 = dir.path().join("hook1.sh");
        fs::write(
            &hook1,
            "#!/bin/bash\necho '{\"action\":\"signal\",\"signal\":\"first_signal\",\"reason\":\"first reason\"}'\n",
        )
        .unwrap();

        let hook2 = dir.path().join("hook2.sh");
        fs::write(
            &hook2,
            "#!/bin/bash\necho '{\"action\":\"signal\",\"signal\":\"second_signal\",\"reason\":\"second reason\"}'\n",
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook1, fs::Permissions::from_mode(0o755)).unwrap();
            fs::set_permissions(&hook2, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let config_path = dir.path().join("hooks.toml");
        fs::write(
            &config_path,
            format!(
                "[[hooks]]\nevent = \"PostToolUse\"\ncommand = \"{}\"\n\n\
                 [[hooks]]\nevent = \"PostToolUse\"\ncommand = \"{}\"\n",
                hook1.display(),
                hook2.display()
            ),
        )
        .unwrap();

        let runner = HookRunner::load(config_path.to_str().unwrap(), dir.path().to_str().unwrap());
        let result = runner
            .run_post_tool_use("Bash", &serde_json::json!({}), "ok", false, 10)
            .await;

        // First signal wins for return value
        match result {
            PostToolResult::Signal { signal, reason } => {
                assert_eq!(signal, "first_signal");
                assert_eq!(reason, "first reason");
            }
            PostToolResult::Continue => panic!("expected signal"),
        }

        // Both observations written (absolute path)
        let conv_path = dir.path().join(".forgeflare/convergence.json");
        let conv = fs::read_to_string(&conv_path).unwrap();
        let state: ConvergenceState = serde_json::from_str(&conv).unwrap();
        assert_eq!(state.observations.len(), 2);
        assert_eq!(state.observations[0].signal, "first_signal");
        assert_eq!(state.observations[1].signal, "second_signal");
    }

    #[test]
    fn phase_none_defaults_to_guard() {
        let hook = HookConfig {
            event: "PreToolUse".to_string(),
            command: "test".to_string(),
            match_tool: None,
            phase: None,
            timeout_ms: None,
        };
        // When filtering guard hooks, None is treated as "guard"
        let phase = hook.phase.as_deref().unwrap_or("guard");
        assert_eq!(phase, "guard");
    }

    #[tokio::test]
    async fn stop_seven_reasons() {
        let reasons = [
            "end_turn",
            "iteration_limit",
            "api_error",
            "continuation_cap",
            "block_limit_consecutive",
            "block_limit_total",
            "convergence_signal",
        ];

        for reason in &reasons {
            let dir = tempfile::tempdir().unwrap();
            let runner = HookRunner::load("/nonexistent", dir.path().to_str().unwrap());
            runner.run_stop(reason, 0, 0).await;

            let conv_path = dir.path().join(".forgeflare/convergence.json");
            let conv = fs::read_to_string(&conv_path).unwrap();
            let state: ConvergenceState = serde_json::from_str(&conv).unwrap();
            assert_eq!(
                state.final_state.as_ref().unwrap().reason,
                *reason,
                "stop reason mismatch for {reason}"
            );
        }
    }
}
