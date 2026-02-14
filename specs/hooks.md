# Hook Dispatch Layer

**Status:** Active
**Target:** Shell-based hook system with three lifecycle events for Ralph loop integration — guard before execution, observe after execution, finalize on stop

---

## Why

Forgeflare's only guardrails are a hardcoded bash deny-list in `bash_exec` and the 50-iteration loop ceiling. Ralph loop hooks (`ralph-guard.sh`, `convergence-check.sh`) run outside forgeflare at the process supervision level — they parse forgeflare's stdout for patterns, which is lossy and delayed. They can't see individual tool calls as structured data.

Moving hooks inside forgeflare gives them the actual tool input and output as JSON. `ralph-guard.sh` sees the exact bash command before execution instead of scraping terminal output. `convergence-check.sh` sees every tool result with error status instead of pattern-matching stdout. The outer Ralph loop reads a structured convergence state file instead of grepping for signals.

**Supersedes** the "No automatic retry; failures return to user for decision" principle from `coding-agent.md` for hook-blocked tools specifically. Blocked tools produce error ToolResults that the LLM sees and can adapt to, but block counters prevent infinite retry within a single turn.

---

## Requirements

**R1. Three Hook Events**

- `PreToolUse` — fires before tool execution. Guard-phase hooks can allow or block the call. Observe-phase hooks always run (even after a guard blocks) for audit logging.
- `PostToolUse` — fires after tool execution. The hook observes the result and can signal convergence. Signal return triggers inner loop break after the current batch completes (control flow, not just logging).
- `Stop` — fires when the conversation loop ends. Reason is one of seven values: `end_turn`, `iteration_limit`, `api_error`, `continuation_cap`, `block_limit_consecutive`, `block_limit_total`, `convergence_signal`.

**R2. Hook Configuration**

TOML file at `.forgeflare/hooks.toml` (project root):

```toml
[[hooks]]
event = "PreToolUse"
phase = "guard"
match_tool = "Bash"
command = ".claude/hooks/ralph-guard.sh"
timeout_ms = 5000

[[hooks]]
event = "PreToolUse"
phase = "guard"
match_tool = "Edit"
command = ".claude/hooks/edit-guard.sh"
timeout_ms = 5000

[[hooks]]
event = "PreToolUse"
phase = "observe"
command = ".claude/hooks/audit-log.sh"
timeout_ms = 3000

[[hooks]]
event = "PostToolUse"
command = ".claude/hooks/convergence-check.sh"
timeout_ms = 5000

[[hooks]]
event = "Stop"
command = ".claude/hooks/session-end.sh"
timeout_ms = 3000
```

- `match_tool` is optional. Omitted = fires for all tools. Specified = exact string match only for that tool name (not prefix — `"Bash"` does not match a hypothetical `"BashScript"`).
- `phase` applies only to `PreToolUse` hooks. `"guard"` (default) can block; `"observe"` always runs for audit purposes. See R4.
- Multiple guard hooks per event run in declaration order. A guard block short-circuits later guard hooks but does NOT skip observe hooks.
- `timeout_ms` defaults to 5000. Stop hooks default to 3000.
- If no `.forgeflare/hooks.toml` exists, `HookRunner` is a no-op. Zero overhead when unconfigured.

**R3. Hook Contract**

Hooks are executables. JSON on stdin, JSON on stdout. Stderr forwarded to forgeflare's stderr for debugging. Exit code 0 = valid JSON response. Non-zero exit = hook failure.

`tool_iterations` is the inner loop's tool dispatch cycle counter — the same counter used for the 50-iteration ceiling. It increments once per API response cycle (per batch), not per individual tool call. All tools in a single batch receive the same `tool_iterations` value. It is NOT `continuation_count` (from maxtoken-continuation) and does NOT reset on continuation prompts. A batch aborted by block threshold does NOT increment `tool_iterations` — the counter only advances when tool_results are sent to the API.

PreToolUse input (guard phase):

```json
{
  "event": "PreToolUse",
  "phase": "guard",
  "tool": "Bash",
  "input": { "command": "cargo test --release" },
  "tool_iterations": 3,
  "cwd": "/home/user/project"
}
```

PreToolUse input (observe phase, after a guard block):

```json
{
  "event": "PreToolUse",
  "phase": "observe",
  "tool": "Bash",
  "input": { "command": "rm -rf /" },
  "blocked": true,
  "blocked_by": "ralph-guard.sh",
  "block_reason": "destructive command detected",
  "tool_iterations": 3,
  "cwd": "/home/user/project"
}
```

PreToolUse input (observe phase, after guard allows):

```json
{
  "event": "PreToolUse",
  "phase": "observe",
  "tool": "Bash",
  "input": { "command": "cargo test --release" },
  "blocked": false,
  "tool_iterations": 3,
  "cwd": "/home/user/project"
}
```

PreToolUse output (guard phase only — observe hooks return value is ignored):

```json
{ "action": "allow" }
```

```json
{ "action": "block", "reason": "Command matches deny pattern: rm -rf" }
```

PostToolUse input:

```json
{
  "event": "PostToolUse",
  "tool": "Bash",
  "input": { "command": "cargo test --release" },
  "result": "test result: ok. 42 passed; 0 failed",
  "is_error": false,
  "tool_iterations": 3,
  "cwd": "/home/user/project"
}
```

The `result` field is capped at 5120 bytes. Results exceeding 5120 bytes are truncated: first 2560 bytes + `"\n... (truncated for hook, full result: {total_len} bytes)\n"` + last 2560 bytes. Byte boundaries use `floor_char_boundary` to avoid splitting multi-byte UTF-8. This prevents large file reads or verbose test output from dominating hook stdin pipe time.

PostToolUse output:

```json
{ "action": "continue" }
```

```json
{ "action": "signal", "signal": "converged", "reason": "3 consecutive clean test runs" }
```

Stop input:

```json
{
  "event": "Stop",
  "reason": "end_turn",
  "tool_iterations": 7,
  "total_tokens": 45000,
  "cwd": "/home/user/project"
}
```

Stop `reason` values:

| Value | Trigger |
|-------|---------|
| `end_turn` | LLM finished naturally (StopReason::EndTurn) |
| `iteration_limit` | Hit 50-tool ceiling (MAX_TOOL_ITERATIONS) |
| `api_error` | API failure after recovery exhausted |
| `continuation_cap` | Hit 3 MaxTokens continuations without completing |
| `block_limit_consecutive` | Consecutive block counter (3) tripped — likely stuck on one bad command |
| `block_limit_total` | Total block counter (10) tripped — alternating blocked/allowed pattern |
| `convergence_signal` | PostToolUse Signal broke the inner loop |

Stop output:

```json
{ "action": "continue" }
```

Unrecognized `action` values in Stop output (e.g., `"signal"`) are logged and treated as `"continue"`. Stop output is parsed for validation and logging only. The return value does not affect control flow.

**R4. PreToolUse Phase Model**

PreToolUse hooks have a `phase` field: `"guard"` (default) or `"observe"`.

Guard-phase hooks:
- Can return `allow` or `block`.
- Run in declaration order. A block short-circuits remaining guard hooks.
- Are fail-closed: timeout, non-zero exit, invalid JSON → Block.

Observe-phase hooks:
- Always run, even after a guard blocks. Their input includes `blocked`, `blocked_by`, and `block_reason` fields reflecting the guard outcome.
- Return value is ignored (fire-and-forget for audit purposes).
- Are fail-open: timeout, crash, bad JSON → logged, no effect.

Execution order for a single tool:
1. Run guard-phase hooks in declaration order (short-circuit on block)
2. Run observe-phase hooks in declaration order (always, with guard outcome in input)

Error messages for guard-phase failures distinguish hook decisions from hook failures:

- Hook blocks intentionally: `"blocked by {hook_command}: {reason}"` (e.g., `"blocked by ralph-guard.sh: destructive command detected"`)
- Hook times out: `"hook failed: {hook_command} timed out after {timeout_ms}ms (tool blocked by default)"`
- Hook crashes (non-zero exit): `"hook failed: {hook_command} exited with code {code} (tool blocked by default)"`
- Hook returns invalid JSON: `"hook failed: {hook_command} returned invalid JSON (tool blocked by default)"`

The LLM sees a ToolResult error with one of these messages. An intentional block tells the LLM to try a different approach. A hook failure tells the LLM not to bother retrying — the guard is broken, not rejecting the specific input.

**R5. PostToolUse and Stop are Fail-Open**

If a PostToolUse or Stop hook fails (timeout, non-zero exit, invalid JSON), log the failure and proceed. These hooks observe — they don't guard. A broken observer should not stall the loop.

**R6. Block Counters with Inner Loop Break**

Two counters track PreToolUse blocks in the inner loop (alongside `tool_iterations`):

`consecutive_block_count: usize` — catches stuck-on-one-tool loops:
- Increment on every PreToolUse guard block (intentional or failure).
- Reset to 0 on every successful tool dispatch (guard allowed and tool executed).
- Threshold: `MAX_CONSECUTIVE_BLOCKS = 3`.

`total_block_count: usize` — catches alternating blocked/allowed patterns:
- Increment on every PreToolUse guard block (intentional or failure). Never resets within the inner loop.
- Threshold: `MAX_TOTAL_BLOCKS = 10`.

When either counter hits its threshold: log the specific counter that tripped, pop the trailing Assistant message from the conversation (`conversation.pop()` — not `recover_conversation()`, see below), and break. The Stop hook fires with `reason: "block_limit_consecutive"` or `"block_limit_total"` depending on which counter tripped. If both trip simultaneously (e.g., 3 consecutive blocks that also push the total to 10), use `"block_limit_consecutive"` (it fired first).

Why not `recover_conversation()`: at block threshold time, the conversation state is `[..., Assistant(tool_use×N)]` — the assistant response has been pushed but no User tool_results message exists yet. The existing `recover_conversation()` expects to find a trailing User message first, then conditionally pops the Assistant message. It will not handle this state correctly. A direct `conversation.pop()` of the trailing Assistant message is the right operation. `recover_conversation()` keeps its existing contract unchanged.

Block threshold takes unconditional precedence over `signal_break` in both paths. If both conditions are active within the same batch (e.g., tool 1 signals convergence, tools 2-4 are blocked hitting the threshold), the Stop reason is `block_limit_*` and the trailing Assistant message is popped. The `signal_break` flag is never checked.

Both counters reset on new user input (outer loop iteration), same scope as `tool_iterations`.

**R7. PostToolUse Signal as Control Flow**

When `run_post_tool_use` returns `PostToolResult::Signal`, set a `signal_break` flag. After the current batch finishes (all tool_results collected), send the tool_results as the User message, then break the inner loop without looping back to the API. The conversation is in a valid state — tool_use blocks have matching tool_results — so `recover_conversation()` is NOT called. The Stop hook fires with `reason: "convergence_signal"`.

In the sequential path: if tool 2 of 4 signals, tools 3 and 4 still execute (their tool_use blocks need matching results). After the batch, the flag triggers the break.

In the parallel path: `join_all` already committed to running all spawned tools. Signal is detected during the post-`join_all` PostToolUse loop. After the loop, the flag triggers the break.

PostToolUse fires only for tools that were dispatched (guard allowed and tool executed), not for blocked tools. A blocked tool produces an error ToolResult but no PostToolUse event. This is intentional — PostToolUse observes execution outcomes, not guard decisions. PreToolUse observe-phase hooks cover the audit trail for blocked calls.

Multiple PostToolUse hooks matching the same tool run in declaration order (sequentially), same as guard hooks. Every hook that returns `Signal` writes its own observation to `convergence.json` (valuable data for the Ralph loop — e.g., "tests_pass" and "lint_clean" from separate hooks in the same invocation). The first `Signal` controls the return value of `run_post_tool_use` (which determines the `signal_break` flag), but subsequent hooks still execute and their observations are still recorded. `run_post_tool_use` collects all observations during the hook loop, performs a single read-modify-write to `convergence.json` after all hooks complete (appending all observations at once), and returns the first `Signal` encountered, or `Continue` if none signaled.

**R8. Convergence State Protocol**

PostToolUse signals and Stop finalization write to `.forgeflare/convergence.json` with distinct keys:

```json
{
  "observations": [
    { "signal": "clean_test", "reason": "3 consecutive clean test runs", "tool_iterations": 12 },
    { "signal": "stable_output", "reason": "identical grep result 3 times", "tool_iterations": 18 }
  ],
  "final": {
    "reason": "convergence_signal",
    "tool_iterations": 22,
    "total_tokens": 45000,
    "timestamp": "2026-02-13T19:30:00Z"
  }
}
```

- Forgeflare truncates (deletes) any existing `.forgeflare/convergence.json` at startup, before entering the conversation loop. Forgeflare owns this file. The Ralph loop reads it after forgeflare exits but does not manage its lifecycle.
- Create `.forgeflare/` directory via `create_dir_all` before the first convergence write if it does not exist.
- PostToolUse signals append to the `observations` array. Each observation includes `tool_iterations` at the time of signal. Multiple signals from the same batch will have identical `tool_iterations` values (see R3 — `tool_iterations` is per-batch, not per-tool).
- Stop hook writes the `final` key. Write-once per session.
- Convergence writes are atomic via same-directory temp file: serialize to `.forgeflare/convergence.json.tmp`, then `fs::rename` to `.forgeflare/convergence.json`. Both paths must be in the same directory — `fs::rename` fails with `EXDEV` across filesystem boundaries. Do NOT use `std::env::temp_dir()` or `/tmp/` for the temp file. This prevents partial writes on crash (SIGKILL, OOM) from corrupting the file the Ralph loop depends on.
- Convergence read-modify-write: `run_post_tool_use` runs all matching hooks, collecting observations from every hook that returns `Signal`. After all hooks complete, it performs a single read-modify-write: read the existing `.forgeflare/convergence.json` (if any), deserialize to a struct, append all collected observations to the `observations` array, and write back atomically. This is a JSON-level append (read, parse, modify, rewrite), NOT a file-level append (which would produce invalid JSON). If the file does not exist, create the initial structure with an empty `observations` array plus the new observations. One file I/O cycle per `run_post_tool_use` call, regardless of how many hooks signal.
- Convergence write failures (disk full, permission denied, rename failure) are logged as warnings but do not affect the return value of `run_post_tool_use` or the completion of `run_stop`. The signal is the hook's decision; persistence is a separate concern. This extends R5's fail-open philosophy to forgeflare's own convergence I/O, not just hook subprocess failures.
- No race condition: PostToolUse and Stop are sequential (Stop fires after the inner loop exits, after all PostToolUse hooks have completed).

**R9. Hooks Wrap Dispatch, Not Inside It**

`dispatch_tool` remains sync. Hook invocations live in `main.rs` at the call site, wrapping the dispatch:

```text
Sequential path:
  threshold_tripped = false
  for each tool_use block:
    PreToolUse guard phase (async, on tokio runtime) — short-circuit on block
    PreToolUse observe phase (async) — always runs, with guard outcome
    if blocked:
      error result, increment block counters
      if consecutive or total block threshold hit:
        threshold_tripped = true
        break (stop evaluating remaining tools)
      continue (skip dispatch)
    dispatch_tool (sync, existing code)
    reset consecutive_block_count = 0
    PostToolUse (async, on tokio runtime)
    if signal → set signal_break flag
  if threshold_tripped:
    conversation.pop() + break (reason: block_limit_*)
    (tool_results discarded — batch abandoned, no message sent)
  else:
    send tool_results
    if signal_break → break (no recover_conversation)

Parallel path (all-pure batch):
  Pre-allocate Vec<Option<ContentBlock>> with capacity = batch size
  blocked_flags: Vec<bool> = vec![false; batch_size]
  threshold_tripped = false
  for each tool_use block (sequentially, before any spawning):
    PreToolUse guard phase (async) — short-circuit on block
    PreToolUse observe phase (async) — always runs
    if blocked:
      fill slot[i] with error result, blocked_flags[i] = true, increment block counters
      if consecutive or total block threshold hit:
        threshold_tripped = true
        break (stop evaluating remaining tools)
    else:
      reset consecutive_block_count = 0
      spawn_blocking(dispatch_tool), collect future with slot index
  if threshold_tripped:
    join_all already-spawned futures, fill their slots (avoid detaching JoinHandles)
    conversation.pop() + break (reason: block_limit_*)
    (filled slots are not consumed — batch enters recovery, no tool_results sent)
  else:
    join_all spawned futures
    fill remaining slots from join_all results (in order)
    for each (slot, blocked) in slots.zip(blocked_flags) (sequentially):
      if blocked → skip (no PostToolUse for blocked tools)
      PostToolUse (async)
      if signal → set signal_break flag
    send tool_results
    if signal_break → break (no recover_conversation)
```

Key parallel path semantics:

- **Block counter check is per-tool, before each `spawn_blocking`.** If the threshold trips mid-batch, remaining tools are not evaluated. Already-spawned tools complete via `join_all` to avoid detaching `JoinHandle` futures — their results are discarded (no side effect concern since the parallel path only runs for all-pure batches per the tool-parallelism spec). The PostToolUse loop is skipped for the entire batch — `block_limit` takes unconditional precedence over `signal_break`. `conversation.pop()` removes the trailing Assistant message, so no tool_results are sent for this batch.
- **`consecutive_block_count` resets at guard-allow time**, not at execution-completion time. In the parallel path, guard allow and tool execution are temporally separated (guard runs before spawn, execution completes after `join_all`). Resetting at guard-allow is correct — the guard allowing a tool breaks the consecutive-block pattern regardless of whether the tool subsequently succeeds or fails at execution.
- **Unfilled `None` slots:** when the batch completes normally (no threshold trip), every slot is filled exactly once — either from the block path or from `join_all`. An unfilled slot after a normal batch is a bug — panic is correct. When the threshold trips mid-batch, the collection/unwrap step is skipped entirely (the batch enters recovery mode).

The pre-allocated slots approach preserves tool_use ordering. Each slot gets filled exactly once — either from the block path or from `join_all`. No post-hoc splicing.

`dispatch_tool` signature is unchanged. `HookRunner` is passed to main.rs, not to `dispatch_tool`. The coupling to main.rs is the right trade-off — the alternative (async dispatch_tool) is incompatible with the `spawn_blocking` approach in tool-parallelism.

---

## Architecture

```text
Startup:
  HookRunner::load(".forgeflare/hooks.toml", &cwd)
  │  missing file → empty runner (no-op)
  Delete .forgeflare/convergence.json if exists (clean slate)

Inner loop:
  tool_use blocks from assistant response
  │
  ├─ classify batch → all Pure?
  │    ├─ yes → parallel path (R9)
  │    └─ no  → sequential path (R9)
  │
  ├─ block threshold hit mid-batch?
  │    └─ yes → conversation.pop() + break (reason: block_limit)
  │              (tool_iterations NOT incremented — aborted batch)
  │
  ├─ tool_results sent back as User message
  ├─ tool_iterations += 1
  │
  └─ signal_break set?
       └─ yes → break (reason: convergence_signal, NO recover)

Loop exit:
  hooks.run_stop(reason, tool_iterations, total_tokens)
  │
  └─ writes "final" key to convergence.json (atomic)
```

Changes to existing code:

1. New: `src/hooks.rs` — `HookRunner`, `HookConfig`, hook I/O types, subprocess execution, convergence state file management, atomic write helper. ~250 LOC.
2. `src/main.rs` — Initialize `HookRunner` at startup, delete stale convergence.json. Wrap tool dispatch (both sequential and parallel paths) with PreToolUse/PostToolUse calls. Add `consecutive_block_count`, `total_block_count`, `signal_break` variables. Call `run_stop` at loop exit with appropriate reason. ~60 LOC delta.
3. `Cargo.toml` — add `toml` dependency for hooks.toml parsing.

What does NOT change:

- `src/tools/mod.rs` — `dispatch_tool` signature, tool exec functions, `tools!` macro, `ToolEffect` enum (from tool-parallelism) all unchanged.
- `src/api.rs` — no changes.
- `src/session.rs` — no changes.

---

## HookRunner Interface

```rust
pub struct HookRunner {
    hooks: Vec<HookConfig>,
    cwd: String,
}

pub struct HookConfig {
    pub event: String,
    pub command: String,
    pub match_tool: Option<String>,
    pub phase: Option<String>,       // PreToolUse only: None → "guard", "observe" explicit
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

pub enum PreToolResult {
    Allow,
    Block { reason: String, blocked_by: String },
}

pub enum PostToolResult {
    Continue,
    Signal { signal: String, reason: String },
}

impl HookRunner {
    /// Load hooks from TOML config. Missing file → empty runner (all methods are no-ops).
    pub fn load(config_path: &str, cwd: &str) -> Self;

    /// Delete convergence.json if it exists. Called once at startup.
    pub fn clear_convergence_state(&self);

    /// Run PreToolUse hooks: guard phase (fail-closed), then observe phase (fail-open).
    /// Returns the guard outcome. Observe hooks always run with guard context.
    pub async fn run_pre_tool_use(
        &self, tool: &str, input: &Value, tool_iterations: usize,
    ) -> PreToolResult;

    /// Run PostToolUse hooks. Fail-open: timeout/crash/bad JSON → Continue.
    /// Writes signals to convergence.json observations array (atomic).
    /// Result string is capped at 5KB before being passed to hooks.
    pub async fn run_post_tool_use(
        &self, tool: &str, input: &Value, result: &str, is_error: bool, tool_iterations: usize,
    ) -> PostToolResult;

    /// Run Stop hooks. Fail-open. Writes final key to convergence.json (atomic).
    pub async fn run_stop(&self, reason: &str, tool_iterations: usize, total_tokens: u64);
}
```

When `hooks` is empty (no config file or no hooks for the event), all methods return immediately: `PreToolResult::Allow`, `PostToolResult::Continue`, `run_stop` is a no-op. The same applies when hooks exist but none match the current tool (e.g., guard hooks configured for `"Bash"` when the tool is `"Read"`): no matching guard hooks → `Allow`. Observe hooks for that tool receive `blocked: false` with `blocked_by` and `block_reason` absent from the JSON input.

---

## Security Considerations

Hook commands execute via `bash -c` with the command string from `.forgeflare/hooks.toml`. This is the same threat model as Makefile, `.vscode/tasks.json`, or any project-root executable config — whoever controls the project root controls hook execution. A malicious repository containing a `.forgeflare/hooks.toml` gets arbitrary code execution when a user clones and runs forgeflare.

Mitigations:
- Recommend adding `.forgeflare/` to `.gitignore` for projects where hooks are operator-specific (not shared).
- For shared hooks (team conventions), hooks.toml is committed intentionally and reviewed like any executable config.
- Hook processes inherit forgeflare's full environment, including `ANTHROPIC_API_KEY` if set. This is acknowledged and accepted — sandboxing hook env is a non-goal.

---

## Success Criteria

- [ ] PreToolUse guard hook blocks a Bash command and returns error ToolResult with hook's reason
- [ ] PreToolUse guard hook timeout blocks the tool with distinct "timed out" error message
- [ ] PreToolUse guard hook crash (non-zero exit) blocks the tool with distinct "exited with code" error message
- [ ] PreToolUse guard hook invalid JSON blocks the tool with distinct "invalid JSON" error message
- [ ] PreToolUse observe hook runs after a guard block and receives `blocked: true` with block context
- [ ] PreToolUse observe hook runs after a guard allow and receives `blocked: false`
- [ ] PreToolUse observe hook failure is logged but does not affect tool dispatch
- [ ] PostToolUse hook observes tool result and logs convergence signal
- [ ] PostToolUse hook failure is logged but does not block the loop
- [ ] PostToolUse Signal sets `signal_break` flag; inner loop breaks after batch completes without `recover_conversation()`
- [ ] Stop hook fires with correct reason for all seven stop conditions (end_turn, iteration_limit, api_error, continuation_cap, block_limit_consecutive, block_limit_total, convergence_signal)
- [ ] Stop hook failure is logged but does not prevent process exit
- [ ] Unrecognized Stop output action is logged and treated as continue
- [ ] Consecutive block counter (3) triggers inner loop break with `conversation.pop()` (not `recover_conversation()`)
- [ ] Total block counter (10) triggers inner loop break with `conversation.pop()` (not `recover_conversation()`)
- [ ] Consecutive block counter resets on successful tool dispatch
- [ ] Total block counter does NOT reset within inner loop
- [ ] Both counters reset on new user input (outer loop)
- [ ] Convergence state written to `.forgeflare/convergence.json` with distinct `observations`/`final` keys
- [ ] Convergence writes are atomic (write-to-temp-then-rename)
- [ ] Stale convergence.json deleted on forgeflare startup
- [ ] `.forgeflare/` directory created via `create_dir_all` before first convergence write
- [ ] PostToolUse `result` field capped at 5KB with truncation marker
- [ ] No hooks.toml → all hook methods are no-ops, zero overhead
- [ ] `dispatch_tool` remains sync — signature unchanged
- [ ] Parallel path block counter check runs per-tool before each `spawn_blocking`
- [ ] Parallel path block threshold mid-batch: already-spawned tools complete, PostToolUse skipped, recovery fires
- [ ] Parallel path `consecutive_block_count` resets at guard-allow time, not execution-completion time
- [ ] Parallel path preserves tool_use ordering with pre-allocated slots (normal batch, no threshold trip)
- [ ] `match_tool` uses exact string matching
- [ ] Sequential path: block threshold check fires inside the blocked branch (after counter increment, before `continue`)
- [ ] Block threshold uses `conversation.pop()` (not `recover_conversation()`) — conversation state is `[..., Assistant(tool_use)]` with no trailing User message
- [ ] Sequential path uses `threshold_tripped` flag pattern — `conversation.pop()` and tool_results discard happen after the for loop, not inside it
- [ ] Convergence write failures logged as warnings, do not affect `run_post_tool_use` return value or `run_stop` completion
- [ ] Block threshold takes precedence over `signal_break` in both paths
- [ ] Multiple PostToolUse hooks for same tool run in declaration order; first Signal wins for return value
- [ ] Every PostToolUse hook returning Signal writes its own observation to convergence.json
- [ ] `run_post_tool_use` performs a single read-modify-write after all hooks complete (not per-hook)
- [ ] Parallel path uses `blocked_flags: Vec<bool>` to skip blocked slots in PostToolUse loop
- [ ] `clear_convergence_state` logs warning on non-NotFound errors and proceeds (no panic, no Result)
- [ ] No matching guard hooks for a tool → `PreToolResult::Allow`
- [ ] Aborted batch (block threshold) does NOT increment `tool_iterations`
- [ ] `phase` field filtering: `None` treated as `"guard"` for PreToolUse only, ignored for PostToolUse/Stop
- [ ] Existing tests pass (dispatch_tool not modified)
- [ ] Hook stderr forwarded to forgeflare stderr

---

## Non-Goals

- Trait-based tool registry or plugin system (5 tools don't need it; separate proposal if forgeflare grows to 10+)
- In-process extensions or dynamic tool registration (shell hooks are the right abstraction for Ralph agents)
- More than three hook events (PreToolUse, PostToolUse, Stop cover the Ralph use case completely)
- PostToolUse result modification (observe-only; modifying what the agent sees causes divergence from reality)
- Hot-reloading of hooks.toml (Ralph loop restarts forgeflare each iteration; config is read once at startup)
- Long-running hook processes with stdin/stdout line protocol (spawn-per-invocation is ~5-10ms on macOS; 100 spawns per turn = ~500ms-1s overhead; acceptable for v1)
- Configurable block counter thresholds via CLI (3 consecutive / 10 total are the right numbers)
- Hook chaining or composition (multiple hooks per event run independently in declaration order; no data passing between hooks)
- Sandboxing hook process environment (hooks inherit forgeflare's full env; accepted trade-off)
- Prefix matching for `match_tool` (exact string only; prefix matching is a footgun)

---

## Implementation Notes

- `HookRunner::load` uses `std::fs::read_to_string` + `toml::from_str`. If the file doesn't exist, return `HookRunner { hooks: vec![], cwd }`. No error on missing file — unconfigured is the default.
- Hook subprocess execution: `tokio::process::Command::new("bash").arg("-c").arg(&hook.command)` with stdin piped, stdout piped, stderr inherited. `tokio::time::timeout` wraps the entire spawn-write-read sequence.
- Hook output parsing: `serde_json::from_str::<HookOutput>(&stdout)`. On parse failure, PreToolUse guard → Block (fail-closed), PreToolUse observe → logged (fail-open), PostToolUse → Continue (fail-open).
- Convergence writes use atomic rename: `serde_json::to_string_pretty` → write to `.forgeflare/convergence.json.tmp` → `fs::rename` to `.forgeflare/convergence.json`. Create `.forgeflare/` via `create_dir_all` on first write. POSIX rename is atomic within the same filesystem.
- `clear_convergence_state` at startup: `fs::remove_file(".forgeflare/convergence.json")`. Ignore `NotFound` (normal — no stale file). For other errors (e.g., permission denied), log a warning and proceed — a stale convergence.json is non-fatal since the first convergence write will overwrite it via atomic rename. The function returns `()`, no error propagation.
- The parallel path pre-allocates `Vec<Option<ContentBlock>>` with `vec![None; batch_size]`. PreToolUse guard blocks fill slots directly. Block counter check runs per-tool, before each `spawn_blocking` — if the threshold trips, remaining tools are not evaluated. Already-spawned futures complete via `join_all`, their slots are filled, then `conversation.pop() + break` fires. The `unwrap()` collection step and PostToolUse loop are both skipped — the batch enters recovery mode. When the batch completes normally (no threshold trip), all slots are filled (either from blocks or `join_all`). `.into_iter().map(|slot| slot.unwrap())` produces the final ordered results. An unfilled slot after a normal batch is a bug — panic is correct.
- The `consecutive_block_count` and `total_block_count` variables live alongside `tool_iterations` in the inner loop scope. `consecutive_block_count` resets at guard-allow time (when a tool passes the guard phase), not at execution-completion time. In the parallel path, this means the reset happens during the guard loop, before `spawn_blocking`. This is correct — the guard allowing a tool breaks the consecutive-block pattern regardless of execution outcome. `total_block_count` never resets within the inner loop. Both counters reset on new user input (outer loop). The consecutive counter catches rapid-fire blocks (LLM retrying the same bad command). The total counter catches alternating blocked/allowed patterns (half the tools being blocked across 20+ iterations is not productive).
- PostToolUse `result` truncation: `result.len()` returns byte count. If `> 5120`, take `result[..floor_char_boundary(2560)]` + truncation marker + `result[floor_char_boundary(result.len()-2560)..]`. The full result still goes to the LLM via the ToolResult — only the hook stdin copy is truncated.
- `phase` filtering: `HookConfig.phase` is `Option<String>` (serde leaves it `None` when omitted from TOML). When filtering PreToolUse hooks by phase, treat `None` as `"guard"`. For PostToolUse and Stop hooks, the `phase` field is irrelevant — do NOT apply a serde default of `"guard"` to the struct (it would incorrectly tag non-PreToolUse hooks). Filter by event first, then by phase only for PreToolUse.
- PreToolUse observe hooks receive a superset of the guard input: same `event`, `tool`, `input`, `tool_iterations`, `cwd` fields, plus `blocked: bool`, `blocked_by: Option<String>`, `block_reason: Option<String>`. When `blocked` is false, `blocked_by` and `block_reason` are absent. Observe hooks' stdout is read and discarded — return value does not affect control flow.
- The `signal_break` flag is a `bool` set during PostToolUse processing. It persists for the remainder of the batch. After sending tool_results, the flag is checked: if true, break without `recover_conversation()` (conversation state is valid). The Stop hook receives `reason: "convergence_signal"`.
- Process spawn overhead: each hook invocation is a `tokio::process::Command` spawn (~5-10ms on macOS, ~1-2ms on Linux). With the observe phase added, worst case per tool is 3 spawns (guard + observe + PostToolUse) instead of 2. For a 50-iteration loop: ~150 spawns = ~750ms-1.5s total on macOS. Acceptable for v1. If profiling shows hooks dominating wall-clock time, the fix is a long-running hook process with line-delimited JSON — but that's a separate spec.
- All references use structural landmarks (function names, match patterns) rather than line numbers, since earlier specs in the implementation order modify the same files.
