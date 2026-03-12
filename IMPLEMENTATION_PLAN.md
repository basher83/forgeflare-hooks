# Implementation Plan

Phase 1: 9/9 complete. Phase 2: 4/5 complete. 151 tests pass, clippy clean, fmt clean.

Updated 2026-03-11: Phase 2 item 2 (SSE Buffer Optimization) complete. Replaced two `.to_string()` allocations with slice borrow + `drain()` in `parse_sse_stream`. All 6 SSE parser tests pass unchanged.
Updated 2026-03-12: Phase 2 item 4 (Project Instructions Loading) complete. InstructionsResult enum, load_project_instructions() function, wired into main() with verbose/warn logging, 11 new tests (CLAUDE.md load, AGENTS.md fallback, priority, symlinks, oversized skip, unreadable skip, both skipped, prompt format, signature unchanged). All 151 tests pass, clippy clean, fmt clean.

## Phase 2 — Active (ordered by priority)

### 1. Glob Shell Injection Fix — `glob-shell-injection.md`
- **Status**: Complete (v0.0.8)
- **Priority**: CRITICAL (security — unguarded arbitrary code execution)
- **Location**: `src/tools/mod.rs` glob_exec, `Cargo.toml`
- **Changes made**:
  - Added `glob = "0.3"` dependency
  - Rewrote `glob_exec` to use `glob::glob()` — no bash subprocess, no shell injection surface
  - Added `expand_braces()` function for `{a,b}` pattern preprocessing (single top-level group)
  - Dedup via `HashSet`, 1000-entry cap, alphabetical ordering from glob crate
  - Shell metacharacters (`$(cmd)`, backticks, `;rm`) become literal pattern characters — never executed
  - 16 new tests: brace expansion unit tests, shell metachar safety, path parameter, result cap, structural no-bash verification
  - All 135 tests pass, clippy clean, fmt clean

### 2. SSE Buffer Optimization — `sse-buffer-optimization.md`
- **Status**: Complete (v0.0.9)
- **Priority**: Low (correctness OK, performance improvement)
- **Location**: `src/api.rs` parse_sse_stream
- **Changes made**:
  - Replaced `buffer[..pos].to_string()` with `&buffer[..pos]` (slice borrow, zero allocation)
  - Replaced `buffer = buffer[pos + 2..].to_string()` with `buffer.drain(..pos + 2)` (in-place memmove)
  - Scoped block `{ }` around the slice borrow so the borrow is dropped before `drain` mutates `buffer`
  - All 135 tests pass, clippy clean, fmt clean

### 3. Prompt Caching — `prompt-caching.md`
- **Status**: Complete (v0.0.10)
- **Priority**: Medium (cost reduction — response-side infrastructure already exists)
- **Location**: `src/api.rs` send_message (lines 140-150), `src/main.rs` run_turn (~line 418)
- **Changes made**:
  - System prompt sent as content block array with `cache_control: {"type": "ephemeral"}`
  - Last tool definition gets `cache_control` at send time (schemas unmodified)
  - Verbose cache stats logging: read, created, total input
  - Removed redundant `content-type` header (overwritten by `.json()`)
  - 5 new tests: system prompt format, last-tool-only cache_control, single tool, empty tools, tool schemas have no cache_control
  - All 140 tests pass, clippy clean, fmt clean

### 4. Project Instructions Loading — `project-instructions-loading.md`
- **Status**: Complete (v0.0.11)
- **Priority**: Medium (agent quality — currently has no project context)
- **Location**: `src/main.rs` (new enum + function + call site in main())
- **Changes made**:
  - New `InstructionsResult` enum: `Found { filename, contents }`, `Skipped { filename, reason }`, `NotFound`
  - New `load_project_instructions()` function: iterates `["CLAUDE.md", "AGENTS.md"]`, first match wins, 32KB size limit, permission checks
  - Wired into `main()` after `build_system_prompt()`, concatenates with section header if found
  - Verbose logging on Found/NotFound, warn-level on Skipped (not verbose-gated)
  - Only cwd searched, no parent traversal
  - 11 new tests: CLAUDE.md loaded, AGENTS.md fallback, priority when both exist, symlinks, >32KB skip, unreadable skip, both skipped, oversized falls through, prompt format, signature unchanged
  - All 151 tests pass, clippy clean, fmt clean

### 5. Run Turn Refactor — `run-turn-refactor.md`
- **Status**: Not started
- **Priority**: Medium (structural debt + 2 bug fixes)
- **Location**: `src/main.rs` run_turn (lines 301-779, currently 479 lines, target <350)
- **Problem**: Parallel and sequential dispatch paths duplicate pre-hook/post-hook/threshold/null-input logic. Two pre-existing bugs:
  - **Bug 1** (line 668-670): Sequential path silently `continue`s on null-input tools — no ToolResult produced, violating API's tool_use/tool_result pairing requirement
  - **Bug 2** (line 509-519): Parallel path doesn't set `blocked_flags[i]` for null-input tools, causing post-hooks to fire on fabricated error results for tools that never executed
- **Changes required**:
  - Extract `run_pre_dispatch()` → returns `PreDispatchResult` enum (`Allow`, `Blocked(ContentBlock)`, `ThresholdTripped`)
  - Extract `run_post_dispatch()` → returns `bool` (signal_break)
  - Both bugs fixed by unified null-input handling in `run_pre_dispatch`
  - Parallel path: pre-dispatch loop → join_all → post-dispatch loop (skip blocked)
  - Sequential path: interleaved loop (pre-dispatch → dispatch → post-dispatch)
  - `dispatch_tool` call itself NOT extracted (parallel=spawn_blocking, sequential=streaming callback — fundamental asymmetry)
- **Tests**: All 119 existing tests must pass with zero modification, clippy clean, run_turn under 350 lines
- **Dependencies**: None, but implement last (largest change surface, touches same code as items 3-4)

## Pre-existing Bugs (confirmed via code search)

These are NOT Phase 2 spec items but bugs found during audit:

1. **Null-input silent skip in sequential path** (`main.rs:668-670`): `continue` produces no ToolResult. The API expects a tool_result for every tool_use. Fixed by item 5.
2. **Missing blocked_flags for null-input in parallel path** (`main.rs:513-519`): Post-hooks fire on error results for tools that never executed. Fixed by item 5.
3. **Tool input JSON parse failure silently swallowed** (`api.rs:291-295`): `if let Ok(v)` silently drops parse errors, leaving ToolUse with `input: {}`. Not addressed by any spec.
4. **`recover_conversation` can empty the conversation** (`main.rs:139-145`): Cascading pops have no minimum-length guard. Not addressed by any spec.

## Completed Items (Phase 1)

1. Foundation: `coding-agent.md` + `tool-name-compliance.md` — CLI, streaming API, 5 PascalCase tools
2. API Endpoint Configuration: `api-endpoint.md` — configurable URL, optional API key
3. API Retry: `api-retry.md` — exponential backoff, error classification
4. Session Capture: `session-capture.md` — JSONL transcripts, usage parsing
5. MaxTokens Continuation: `maxtoken-continuation.md` — 3-attempt cap, classify_max_tokens
6. Token-Aware Trim: `token-aware-trim.md` — 120K threshold gating
7. Tool Parallelism: `tool-parallelism.md` — ToolEffect enum, batch classification
8. Hook Dispatch: `hooks.md` — guard/observe/post/stop, fail-closed guards, convergence writes
9. Release Workflow: `release-workflow.md` — tag-triggered, macOS aarch64 + Linux x86_64, pinned SHAs

## Spec Errata (documented during Phase 1 implementation)

- `release-workflow.md` line 77: success criteria says "working `agent` binary" — should say `forgeflare`. The workflow correctly uses `forgeflare`.
- `session-capture.md` JSONL example (line 101): uses snake_case `read_file` in tool_use name. Should be PascalCase `Read` per `tool-name-compliance.md`. Cosmetic only.

## Minor Code Observations (not blocking, no spec needed)

- `tool_effect` function name in code vs `classify_effect` in CLAUDE.md (naming mismatch, cosmetic)
- `MAX_CONTINUATIONS` constant defined at `main.rs:790`, separated from other constants at lines 15-23
- `stop_reason_str` is stringly-typed (`&str`) where the rest of the codebase uses typed enums
- `which rg` check in `grep_exec` re-runs on every Grep call (no caching)
- Edit 100KB size limit only applies to the replace path, not create/append
- `bash_exec` has no output size cap (unbounded accumulation until 120s timeout)
- Bash schema declares `description` parameter that is never read by `bash_exec`

## Learnings

- `futures_util::stream::once` produces non-Unpin streams; use `stream::iter` for test mocks
- Test SSE data must have proper JSON escaping — `partial_json` values need complete JSON (including closing braces)
- Grep tests searching "." will match their own source code — use temp directories for no-match assertions
- `std::io::IsTerminal` trait (Rust 1.70+) replaces FFI `isatty` calls
- The `bytes` crate must be an explicit dependency even though reqwest re-exports it
- reqwest needs the `json` feature for `.json()` request builder method
- clippy's `needless_range_loop` lint fires when indexing into an array inside a range loop — use `#[allow]` when the loop range intentionally exceeds the array length (initial call + retries pattern)
- The `AgentError` variants from item 1 already matched the retry spec's error classification needs — designing error types early pays off
- `tempfile` crate needed as dev-dependency for session tests that create temp directories
- `Usage` struct placed in `api.rs` alongside other API types, re-exported to `session.rs` via `crate::api::Usage`
- `StopReason` already had `Serialize` derive from item 1, no change needed (spec said to add it)
- SSE `message_start` carries usage at `message.usage` (nested), while `message_delta` carries usage at top-level `usage`
- Extracted `classify_max_tokens()` with `MaxTokensAction` enum from inline branch logic — makes the MaxTokens decision testable without needing a full API client mock
- `continuation_count` is naturally scoped to `run_turn()` — each call to `run_turn()` gets a fresh count, so outer-loop reset happens for free
- `trim_if_needed()` extracted as a named function rather than inline if/else — makes the token gating testable independently of the full conversation loop
- Moving trim inside the inner loop (before each API call) is correct — the conversation grows between iterations (tool results added), so trim needs to re-evaluate each time
- dispatch_tool's `&mut dyn FnMut(&str)` streaming callback is neither Send nor 'static — parallel path creates a local no-op `&mut |_: &str| {}` inside each spawn_blocking closure. Pure tools don't use streaming output so nothing is lost.
- join_all preserves input ordering — no post-hoc reordering needed for tool result ordering
- Batch classification extracts (id, name, input) tuples before the parallel/sequential branch to avoid borrowing blocks inside both paths
- HookRunner stores absolute convergence paths (dir, path, tmp) to avoid test parallelism issues with set_current_dir — tests that change cwd race with parallel tests
- write_observations and write_final_state take explicit Path parameters rather than using constants — enables isolated temp-dir testing
- Convergence state uses a custom ConvergenceState struct with serde for read-modify-write; the `final` JSON key is mapped to `final_state` (Rust reserved word) via `#[serde(rename = "final")]`
- tokio::process::Command needs explicit stdin close (drop after write_all) for hooks to receive EOF and produce output
- Hook subprocess execution wraps spawn-write-read in tokio::time::timeout — the timeout covers the entire sequence, not just individual operations
- Release workflow uses inline CI validation per matrix leg rather than a separate CI job dependency — simpler and avoids cross-workflow triggers
- The `glob` crate's `glob()` uses `MatchOptions::new()` (case_sensitive: true) while `Default::default()` sets case_sensitive: false — use `glob()` not `glob_with(_, MatchOptions::default())` to match bash behavior
- The `glob` crate does not support brace expansion — implement `expand_braces()` preprocessor for `{a,b}` patterns before calling `glob::glob()`
- Tests using `set_current_dir` must be serialized with a static `Mutex<()>` — cargo test runs tests in parallel within the same process, and cwd is process-global state. The `with_temp_cwd` helper pattern (lock → save original → set temp cwd → run closure → restore original) prevents races.
