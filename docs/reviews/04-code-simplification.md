# Code Simplification Review: ForgeFlare

**Reviewer**: Automated Refactoring Agent
**Date**: 2026-02-26
**Scope**: All source files for simplification and cleanup opportunities

---

## Executive Summary

The ForgeFlare codebase at ~4,900 lines is compact and well-organized. The main simplification opportunities center around the `run_turn` function (478 lines, the single largest complexity hotspot), duplicated patterns in the tool dispatch paths, and several small but impactful string handling improvements. The findings are ordered by impact.

---

## Findings

### 1. Parallel vs Sequential Tool Dispatch Duplication

**Impact**: High
**Location**: `src/main.rs:505-747`

The parallel and sequential tool dispatch paths duplicate significant logic (~240 lines): PreToolUse guard checks, block threshold tracking, tool result construction, post-dispatch logging, and PostToolUse hook calls.

**Simplified version** -- extract shared helpers:

```rust
struct BlockCounters {
    consecutive: usize,
    total: usize,
}

impl BlockCounters {
    fn record_block(&mut self) -> Option<&'static str> {
        self.consecutive += 1;
        self.total += 1;
        if self.consecutive >= MAX_CONSECUTIVE_BLOCKS {
            Some("block_limit_consecutive")
        } else if self.total >= MAX_TOTAL_BLOCKS {
            Some("block_limit_total")
        } else {
            None
        }
    }
    fn record_allow(&mut self) { self.consecutive = 0; }
}

fn make_blocked_result(id: &str, reason: String) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: id.to_string(),
        content: reason,
        is_error: Some(true),
    }
}

fn make_tool_result(id: &str, result: Result<String, String>) -> ContentBlock {
    let (content, is_error) = match result {
        Ok(output) => (output, false),
        Err(err) => (err, true),
    };
    ContentBlock::ToolResult {
        tool_use_id: id.to_string(),
        content,
        is_error: if is_error { Some(true) } else { None },
    }
}
```

Eliminates ~100 lines of near-identical code.

---

### 2. `run_turn` Function Complexity (478 Lines, 8 Params)

**Impact**: High
**Location**: `src/main.rs:301-779`

The function does too many things: outer loop, retry loop, MaxTokens branching, parallel/sequential dispatch, threshold tracking, signal handling.

**Simplified version** -- introduce a context struct:

```rust
struct TurnContext<'a> {
    cli: &'a Cli,
    client: &'a AnthropicClient,
    system_prompt: &'a str,
    tools: &'a [serde_json::Value],
}

impl TurnContext<'_> {
    async fn call_api_with_retry(&self, ...) -> Option<(Vec<ContentBlock>, StopReason, Usage)> { ... }
    async fn dispatch_tools_parallel(&self, ...) -> Vec<ContentBlock> { ... }
    async fn dispatch_tools_sequential(&self, ...) -> Vec<ContentBlock> { ... }
}
```

---

### 3. Duplicated JoinHandle Await Pattern

**Impact**: Medium
**Location**: `src/main.rs:582-623`

The code for awaiting spawned JoinHandles and mapping panics to error results is duplicated identically in the `threshold_tripped` and normal paths.

**Simplified version**: Join all spawned futures unconditionally, then branch for post-processing:

```rust
// Join all spawned futures unconditionally
let handles: Vec<_> = spawn_futures
    .into_iter()
    .map(|(idx, h)| async move { (idx, h.await) })
    .collect();
for (idx, result) in futures_util::future::join_all(handles).await {
    slots[idx] = Some(match result {
        Ok(block) => block,
        Err(_) => ContentBlock::ToolResult {
            tool_use_id: tool_uses[idx].0.clone(),
            content: "tool panicked".to_string(),
            is_error: Some(true),
        },
    });
}

// Then branch on threshold_tripped
if threshold_tripped { Vec::new() } else { /* PostToolUse calls */ }
```

---

### 4. Duplicated Stdout/Stderr Reader Threads

**Impact**: Medium
**Location**: `src/tools/mod.rs:271-308`

Two identical closures with only the pipe variable different.

**Simplified version**:

```rust
fn pipe_reader(pipe: impl std::io::Read + Send + 'static, tx: mpsc::Sender<String>) {
    std::thread::spawn(move || {
        let mut reader = BufReader::with_capacity(4096, pipe);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let s = String::from_utf8_lossy(&buf).to_string();
                    if tx.send(s).is_err() { break; }
                }
            }
        }
    });
}
```

Reduces 30 lines to 15.

---

### 5. Duplicated Convergence State Read-Modify-Write

**Impact**: Medium
**Location**: `src/hooks.rs:481-536`

`write_observations` and `write_final_state` share the same read-deserialize-modify-serialize-atomic-write pattern.

**Simplified version**:

```rust
fn read_convergence_state(path: &Path) -> ConvergenceState {
    fs::read_to_string(path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

fn write_convergence_state(state: &ConvergenceState, dir: &Path, path: &Path, tmp: &Path)
    -> std::io::Result<()>
{
    fs::create_dir_all(dir)?;
    let json = serde_json::to_string_pretty(state).map_err(std::io::Error::other)?;
    fs::write(tmp, &json)?;
    fs::rename(tmp, path)
}
```

Each caller now only contains its unique mutation logic.

---

### 6. SSE Buffer Double-Allocation Per Event

**Impact**: Medium
**Location**: `src/api.rs:209-214`

```rust
// Current: two allocations per event
let event_block = buffer[..pos].to_string();
buffer = buffer[pos + 2..].to_string();
```

**Simplified version**:

```rust
// Process event_block as a borrow, then drain in-place
for line in buffer[..pos].lines() {
    if let Some(data) = line.strip_prefix("data: ") {
        // ... same processing ...
    }
}
buffer.drain(..pos + 2);  // single in-place mutation, no allocation
```

Saves two heap allocations per SSE event.

---

### 7. `truncate_result` Unnecessary Allocation

**Impact**: Low
**Location**: `src/hooks.rs:402-416`

Always allocates a new String even when the result is under the limit.

**Simplified version** using `Cow`:

```rust
fn truncate_result(result: &str) -> Cow<'_, str> {
    if result.len() <= RESULT_TRUNCATION_LIMIT {
        return Cow::Borrowed(result);
    }
    // ... truncation logic ...
    Cow::Owned(format!(...))
}
```

---

### 8. Verbose rg-Existence Check

**Impact**: Low
**Location**: `src/tools/mod.rs:454-470`

Two match arms returning the same error string.

**Simplified version**:

```rust
let rg_available = Command::new("which")
    .arg("rg")
    .output()
    .map(|o| o.status.success())
    .unwrap_or(false);

if !rg_available {
    return Err("ripgrep (rg) is not installed...".to_string());
}
```

---

### 9. Unnecessary `blocked_flags` Vec in Parallel Path

**Impact**: Low
**Location**: `src/main.rs:509`

A separate `blocked_flags: Vec<bool>` is maintained alongside `slots: Vec<Option<ContentBlock>>`. Blocked state is determinable from whether the slot was pre-filled.

**Simplified version**: Check `slots[i].is_some()` before the join instead of maintaining a parallel boolean vector.

---

### 10. `edit_exec` Reads File Before Size Check

**Impact**: Low
**Location**: `src/tools/mod.rs:409-417`

**Current**: `read_to_string` then `metadata().len()` check.
**Fix**: Swap the order. Check size first, then read (consistent with `read_exec`).

---

### 11. `bash_exec` Timeout Loop Simplification

**Impact**: Medium
**Location**: `src/tools/mod.rs:244-372`

**Simplified version** using `saturating_duration_since` and `try_iter()`:

```rust
loop {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        let _ = child.kill();
        timed_out = true;
        break;
    }
    match rx.recv_timeout(remaining.min(Duration::from_millis(50))) {
        Ok(chunk) => {
            stream_cb(&chunk);
            output.push_str(&chunk);
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            for chunk in rx.try_iter() {
                stream_cb(&chunk);
                output.push_str(&chunk);
            }
            let _ = child.wait();
            break;
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            if let Ok(Some(_)) = child.try_wait() {
                for chunk in rx.try_iter() {
                    stream_cb(&chunk);
                    output.push_str(&chunk);
                }
                break;
            }
        }
    }
}
```

---

### 12. `recover_conversation` Nested If-Let

**Impact**: Low
**Location**: `src/main.rs:123-148`

**Simplified version** using `is_some_and()` (stable since Rust 1.70):

```rust
fn recover_conversation(messages: &mut Vec<Message>) {
    if messages.last().is_some_and(|m| m.role == "user") {
        messages.pop();
    }

    let is_orphaned = messages.last().is_some_and(|m| {
        m.role == "assistant" && m.content.iter().all(|b| matches!(b, ContentBlock::ToolUse { .. }))
    });

    if is_orphaned {
        messages.pop();
        if messages.last().is_some_and(|m| m.role == "user") {
            messages.pop();
        }
    }
}
```

---

### 13. `Message.role` Should Be an Enum

**Impact**: Medium
**Location**: `src/api.rs:85`

Using `String` for a two-value type (`"user"` / `"assistant"`) permits invalid states and requires `.to_string()` allocations at every construction site.

**Simplified version**:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Role {
    #[serde(rename = "user")]
    User,
    #[serde(rename = "assistant")]
    Assistant,
}
```

Eliminates a class of bugs and many `.to_string()` allocations.

---

### 14. `from_utf8_lossy().to_string()` Pattern

**Impact**: Low
**Location**: `src/tools/mod.rs:206,280,300,490,491`

`.to_string()` always allocates. `.into_owned()` is more semantically clear and avoids an extra allocation when the `Cow` is already `Owned`.

---

### 15. `cwd` Computed Twice

**Impact**: Low
**Location**: `src/main.rs:53-55` and `src/main.rs:216-218`

The current working directory is computed identically in `build_system_prompt()` and `main()`. Extract a helper or pass as parameter.

---

## Summary Table

| # | Finding | Impact | File | Category |
|---|---------|--------|------|----------|
| 1 | Parallel vs sequential dispatch duplication | **High** | main.rs | Duplicated Logic |
| 2 | `run_turn` 478 lines, 8 parameters | **High** | main.rs | Complex Function |
| 3 | JoinHandle await pattern duplicated | Medium | main.rs | Duplicated Logic |
| 4 | Stdout/stderr reader thread duplication | Medium | tools/mod.rs | Duplicated Logic |
| 5 | Convergence read-modify-write duplication | Medium | hooks.rs | Duplicated Logic |
| 6 | SSE buffer double-allocation per event | Medium | api.rs | String Handling |
| 7 | `truncate_result` always allocates | Low | hooks.rs | String Handling |
| 8 | Verbose rg-existence check | Low | tools/mod.rs | Pattern Simplification |
| 9 | Unnecessary `blocked_flags` Vec | Low | main.rs | Over-Engineering |
| 10 | File read before size check in edit | Low | tools/mod.rs | Pattern Simplification |
| 11 | Complex bash_exec timeout loop | Medium | tools/mod.rs | Complex Function |
| 12 | Nested if-let in recover_conversation | Low | main.rs | Pattern Simplification |
| 13 | `Message.role` String should be enum | Medium | api.rs | Type Safety |
| 14 | `from_utf8_lossy().to_string()` pattern | Low | tools/mod.rs | String Handling |
| 15 | `cwd` computed twice | Low | main.rs | Duplicated Logic |

---

## Recommended Priority

1. **Extract tool dispatch helpers** (1, 3, 9) -- biggest win, ~100 lines reduced
2. **Introduce `TurnContext` struct** (2) -- enables further decomposition
3. **Extract convergence R/W helpers** (5) -- targeted refactor
4. **SSE buffer `drain`** (6) -- performance win, minimal change
5. **Extract `pipe_reader` helper** (4) -- quick cleanup
6. **Fix edit_exec size check ordering** (10) -- correctness fix, one-line swap
7. **Role enum** (13) -- type safety, larger scope
