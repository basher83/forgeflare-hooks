# Implementation Plan

Project state: greenfield. Zero source code exists — no `Cargo.toml`, no `src/` directory, no `.rs` files, no `.github/workflows/`. All 10 specs are authored and complete. The Ralph loop harness (`loop.sh`), AGENTS.md, CLAUDE.md, and mise.toml are configured. Implementation follows the order defined in `specs/README.md` to avoid structural conflicts.

Updated 2026-02-14: Items 1 (Foundation) and 2 (API Endpoint Configuration) are complete. 33 tests pass, clippy clean, fmt clean. Binary compiles and all 5 tools work with PascalCase names.

## 1. Foundation: `coding-agent.md` + `tool-name-compliance.md` (combined)

Depends on: nothing (greenfield starting point).
These two specs must be implemented together — tool-name-compliance is a rename applied during initial tool implementation, not a post-hoc migration.

- [x] **1a. Cargo.toml** — Initialize Rust project with dependencies: `reqwest` (stream), `serde` + `serde_json`, `tokio` (full), `clap` (derive), `thiserror`, `futures-util`. Binary name: `forgeflare`. Edition 2021.
- [x] **1b. `src/main.rs` — CLI and conversation loop** — `Cli` struct with `--verbose`, `--model` (default `claude-opus-4-6`), `--max-tokens` (default 16384). Outer loop: read user input (stdin detection for piped mode). Inner loop: call API → check stop_reason → dispatch tools → send results → repeat. Piped stdin reads single prompt, interactive prompts via readline. Exit on EOF or "exit". `build_system_prompt()` with cwd, platform, tool guidance (PascalCase names: Read, Glob, Bash, Edit, Grep). Context trim at exchange boundaries (720KB budget via `trim_conversation()`). `recover_conversation()` for API error recovery (pop trailing User message + orphaned tool_use to maintain alternation invariant). 50-iteration tool loop safety (`MAX_TOOL_ITERATIONS`). NO_COLOR convention. Tool result visibility (size in non-verbose, errors always shown with 200-char preview). Null-input tool_use filter for MaxTokens truncation (retain only blocks with non-null input, inject placeholder text if empty).
- [x] **1c. `src/api.rs` — Anthropic HTTP client with SSE streaming** — `AnthropicClient` struct with reqwest client. `send_message()` that POSTs to `/v1/messages` with streaming. SSE parser collecting `content_block_delta` events into text/tool_use content blocks. `StopReason` enum (EndTurn, MaxTokens, ToolUse). `AgentError` enum via `thiserror`. `ContentBlock` enum (Text, ToolUse, ToolResult). `Message` struct (with Serialize/Deserialize). Reqwest timeouts: 30s connect, 300s request. Retry-After header parsing on 429 (surfaced in error message string for now; structured extraction comes in item 3).
- [x] **1d. `src/tools/mod.rs` — Five tools with PascalCase names** — `tools!` macro generating `all_tool_schemas()`. Tool names: `Read`, `Glob`, `Bash`, `Edit`, `Grep` (per `tool-name-compliance.md`). `dispatch_tool()` hand-written with match arms for PascalCase names. Streaming callback (`&mut dyn FnMut(&str)`) for Bash. Bash command guard (deny-list: `rm -rf /`, fork bombs, `dd` to devices, `mkfs`, `chmod 777 /`, `git push --force`/`-f`, whitespace-normalized lowercase matching). Edit with exact-match default + `replace_all` optional boolean (error hints at replace_all when duplicates found). Empty old_str on missing file = create with mkdir; empty old_str on existing file = append. Grep shells out to `rg` (actionable error when rg not installed). Schema descriptions with limits (1MB read, 100KB edit, 1000 glob entries, 50 grep matches, 120s bash timeout). Bash streaming via mpsc channels + 50ms polling (reader threads send 4KB chunks, partial output preserved on timeout).
- [x] **1e. Tests** — Unit tests for: tool schemas (5 tools with correct PascalCase names), dispatch known/unknown tool, SSE parser (content_block_delta assembly into Text/ToolUse), system prompt contains environment info (cwd, platform), bash deny-list (normalized matching), edit replace_all behavior, tool result formatting (size display, error preview). Integration tests for conversation flow.
- [x] **1f. Validation** — `cargo fmt --check && cargo clippy -- -D warnings && cargo test && cargo build` must all pass. `<950 production lines` (counted via `find src -name '*.rs' | xargs grep -v '^\s*$' | grep -v '^\s*//' | grep -v '#\[cfg(test)\]' -A9999 | wc -l` or similar — test code excluded).

## 2. API Endpoint Configuration: `api-endpoint.md`

Depends on: item 1 (modifies `AnthropicClient` and `Cli` structs created in item 1).

- [x] **2a. `src/api.rs`** — `AnthropicClient` gains `api_url: String` and `api_key: Option<String>`. `new()` accepts `api_url: &str` (reads `ANTHROPIC_API_KEY` from env internally). `send_message()` uses `format!("{}/v1/messages", self.api_url)`. Conditionally attach `x-api-key` header only when `api_key.is_some()`. Always send `anthropic-version: 2023-06-01`. Remove `MissingApiKey` error variant from `AgentError`.
- [x] **2b. `src/main.rs`** — `Cli` struct gains `--api-url` (env = `ANTHROPIC_API_URL`, default = `https://anthropic-oauth-proxy.tailfb3ea.ts.net`). clap `env` attribute gives three-tier precedence (CLI > env > default) for free. Pass resolved URL to `AnthropicClient::new()`. `--verbose` prints resolved API URL at startup.
- [x] **2c. Tests** — Update any tests referencing old `AnthropicClient::new()` signature. Verify conditional auth header (present when key set, absent when None).

## 3. API Retry: `api-retry.md`

Depends on: items 1-2 (modifies error handling in `api.rs` and the `send_message()` call site in `main.rs`).

- [ ] **3a. `src/api.rs` — Error classification** — Add `HttpError { status: u16, retry_after: Option<u64>, body: String }` variant to `AgentError` (replaces string-stuffed status). Add `StreamTransient(String)` variant for transient stream errors (overload events, connection drops). Keep `StreamParse(String)` for permanent parse failures. Add `classify_error(e: &AgentError) -> ErrorClass` function. `ErrorClass` enum: `Transient`, `Permanent`. Classification: `HttpError` by status (429/503/529/5xx → Transient, 4xx → Permanent); `StreamTransient` → always Transient; `StreamParse` → always Permanent; `Api(reqwest::Error)` → `is_timeout()`/`is_connect()` → Transient, else Permanent; `Json` → always Permanent. SSE error event classification: inspect `p["error"]["type"]` (nested, not top-level `p["type"]`). `overloaded_error`/`api_error`/`rate_limit_error` → `StreamTransient`. `invalid_request_error` → `StreamParse`. Missing stop_reason (connection drop) → `StreamTransient`.
- [ ] **3b. `src/main.rs` — Retry loop** — Replace `match client.send_message().await { Ok(r) => r, Err(e) => ... }` with retry loop wrapping the entire call-and-match. Backoff schedule: `[2, 4, 8, 16]` seconds. Max 4 retries (5 total calls). `retry_after` header from `HttpError` overrides backoff (capped at 60s, 0 = immediate). Log format: `[retry] Attempt {n}/4: {context} — waiting {delay}s`. Permanent errors skip retry (immediate `recover_conversation() + break`). After max retries exhausted: fall through to existing error handling. `[retry] Retrying from beginning of response...` before retrying `StreamTransient`.
- [ ] **3c. Tests** — Test `classify_error` for each variant (HttpError by status, StreamTransient, StreamParse, Api timeout/connect/other, Json). Test backoff schedule values. Test permanent error bypass (no retry). Test retry_after override and cap.

## 4. Session Capture: `session-capture.md`

Depends on: items 1-3 (needs `send_message()` return value, `Usage` struct, and retry loop in place).

- [ ] **4a. `Cargo.toml`** — Add `uuid` (v4 feature) and `chrono` for timestamps.
- [ ] **4b. `src/session.rs` (new module)** — Session ID generation (`{YYYY-MM-DD}-{uuid-v4}`). `SessionWriter` struct. JSONL line wrapper struct with `type` (user/assistant), `sessionId`, `uuid` (per-line unique), `parentUuid` (previous line's uuid, null for first), `timestamp` (UTC ISO 8601), `cwd`, `version` (`env!("CARGO_PKG_VERSION")`), `message` (the existing `Message` struct). `append_user_turn()` and `append_assistant_turn()` methods. `write_prompt()` for prompt.txt. `write_context()` for context.md (session metadata header + key actions list from tool_use blocks: `- **{tool_name}**: {first_arg_value}`). File I/O: `OpenOptions::new().create(true).append(true)` per write (no persistent handle). Directory creation: `create_dir_all(".entire/metadata/{session-id}/")`.
- [ ] **4c. `src/api.rs`** — Parse `usage` from `message_start` (has `input_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`) and `message_delta` (has `output_tokens`) SSE events. `Usage` struct with four `u64` fields. Return as third element: `send_message() -> Result<(Vec<ContentBlock>, StopReason, Usage), AgentError>`. Add `Serialize` to `StopReason` derive list.
- [ ] **4d. `src/main.rs`** — Generate session ID at startup. Create `SessionWriter`. Append user turn after each user input push. Append assistant turn after each assistant response. Write `prompt.txt` on first user input. Write `context.md` at session end (before process exit). Continuation prompts also captured via `session.append_user_turn()`.
- [ ] **4e. Tests** — JSONL format validation (each line is valid JSON via `serde_json::from_str`). Session ID format regex (`\d{4}-\d{2}-\d{2}-[0-9a-f-]{36}`). Timestamp presence and ISO 8601 format. `parentUuid` chaining (each line's parentUuid = previous line's uuid). Usage parsing from mock SSE events (message_start + message_delta).

## 5. MaxTokens Continuation: `maxtoken-continuation.md`

Depends on: items 1-4 (restructures inner loop control flow; needs session capture for continuation prompt logging).

- [ ] **5a. `src/main.rs` — Control flow restructure** — Add `let mut continuation_count: usize = 0;` alongside `tool_iterations`. Replace `if stop_reason != StopReason::ToolUse { break; }` with canonical three-way branch:
  - `EndTurn` → break
  - `MaxTokens` → filter null-input tool_use blocks (existing), inject placeholder if empty (existing); then: if empty after filter (only placeholder, no real content) → break; if has valid tool_use blocks → fall through to dispatch (do NOT increment continuation_count); else if `continuation_count < 3` → increment, push User("Continue from where you left off."), `session.append_user_turn()`, log `[continue]`, continue inner loop; else → log `[continue] Max continuations reached`, break
  - `ToolUse` → fall through to dispatch
  - `continuation_count` resets on outer loop user input only (not on tool_result pushes within inner loop). Does NOT increment `tool_iterations`.
- [ ] **5b. Tests** — Text-only MaxTokens triggers continuation (count increments, User message pushed). Tool_use MaxTokens falls through to dispatch (continuation_count unchanged). Cap enforcement (4th attempt breaks). Empty MaxTokens (placeholder only) breaks immediately. Counter reset on new user input (outer loop).

## 6. Token-Aware Trim: `token-aware-trim.md`

Depends on: items 1-4 (needs `Usage.input_tokens` from `send_message()` return value).

- [ ] **6a. `src/main.rs`** — Add `let mut last_input_tokens: u64 = 0;` alongside `tool_iterations`. After successful `send_message()` (after retry loop), update: `last_input_tokens = usage.input_tokens`. Before `trim_conversation()` call, gate: `last_input_tokens == 0` → run trim (no data yet); `last_input_tokens > 0 && < 120_000` → skip trim; `last_input_tokens >= 120_000` → run trim. Constants: `const MODEL_CONTEXT_TOKENS: u64 = 200_000;` and `const TRIM_THRESHOLD: u64 = 120_000;` (60% of context). Resets to 0 on new user input (outer loop). Updated after every successful API call including continuations.
- [ ] **6b. Tests** — First call (last_input_tokens == 0) runs byte trim. Subsequent call with usage < 120K skips trim. Subsequent call with usage >= 120K runs trim. Reset on outer loop iteration.

## 7. Tool Parallelism: `tool-parallelism.md`

Depends on: items 1-6 (modifies tool dispatch block; independent of control flow changes in items 5-6 but must come after them in implementation order).

- [ ] **7a. `src/tools/mod.rs`** — Add `pub enum ToolEffect { Pure, Mutating }` and `pub fn tool_effect(name: &str) -> ToolEffect`. Mapping: `"Read"/"Glob"/"Grep"` → `Pure`, `"Bash"/"Edit"` → `Mutating`, unknown → `Mutating`. Import in main.rs alongside `all_tool_schemas` and `dispatch_tool`.
- [ ] **7b. `src/main.rs`** — Before the `for block in ...` tool dispatch loop, classify the entire batch via `tool_effect()`. If all `Pure`: parallel path using `futures_util::future::join_all` with `tokio::task::spawn_blocking`. Clone `name`, `input`, `id` into each closure (owned copies). No-op streaming callback inside each closure (`&mut |_: &str| {}`). Null-input guard inside each closure (preserves position ordering). Pre-dispatch logging on main thread before spawning. Collect `Vec<(String, ContentBlock)>` tuples from `join_all` for post-dispatch logging (name + result). `JoinError` (thread panic) → `ContentBlock::ToolResult` error with `id_fallback`. If any `Mutating`: entire batch sequential (existing for-loop, unchanged). `tool_iterations += 1` after BOTH paths. `tool_results.is_empty()` break guard after both paths.
- [ ] **7c. Tests** — 3 concurrent Reads complete faster than sequential. Mixed batch (Read + Edit) dispatches sequentially. Tool result ordering preserved. Individual tool errors don't cancel siblings. `ToolEffect` classification exhaustive for all 5 tools. Unknown tool → `Mutating`. Batch of 1 pure tool works correctly.

## 8. Hook Dispatch: `hooks.md`

Depends on: items 1-7 (wraps both sequential and parallel dispatch paths; depends on final shape of tool dispatch from item 7).

- [ ] **8a. `Cargo.toml`** — Add `toml` dependency for hooks.toml parsing.
- [ ] **8b. `src/hooks.rs` (new module, ~250 LOC)** — `HookRunner` struct with `hooks: Vec<HookConfig>` and `cwd: String`. `HookConfig` with `event: String`, `command: String`, `match_tool: Option<String>`, `phase: Option<String>` (None treated as "guard" for PreToolUse only), `timeout_ms: u64` (default 5000, Stop default 3000). `PreToolResult` enum: `Allow`, `Block { reason, blocked_by }`. `PostToolResult` enum: `Continue`, `Signal { signal, reason }`. `HookRunner::load(config_path, cwd)` — `read_to_string` + `toml::from_str`, missing file → empty runner. `clear_convergence_state()` — delete `.forgeflare/convergence.json`, ignore NotFound, log warning on other errors. `run_pre_tool_use(tool, input, tool_iterations)` — filter matching hooks by event+match_tool, run guard phase (declaration order, short-circuit on block, fail-closed: timeout/crash/bad JSON → Block with distinct messages), then observe phase (always runs with guard outcome context: `blocked`, `blocked_by`, `block_reason` fields; fail-open). `run_post_tool_use(tool, input, result, is_error, tool_iterations)` — fail-open, result field capped at 5120 bytes (first 2560 + truncation marker + last 2560, `floor_char_boundary` for UTF-8), run all matching hooks, collect Signal observations, single read-modify-write to convergence.json `observations` array after all hooks complete, return first Signal or Continue. `run_stop(reason, tool_iterations, total_tokens)` — fail-open, writes `final` key to convergence.json. Hook subprocess: `tokio::process::Command::new("bash").arg("-c").arg(&command)`, stdin piped (JSON), stdout piped (JSON), stderr inherited. `tokio::time::timeout` wraps spawn-write-read sequence. Convergence writes atomic: serialize → `.forgeflare/convergence.json.tmp` → `fs::rename` (same directory, no EXDEV). `create_dir_all(".forgeflare/")` before first write. Write failures logged as warnings, do not affect return values.
- [ ] **8c. `src/main.rs`** — Initialize `HookRunner::load(".forgeflare/hooks.toml", &cwd)` at startup. Call `hooks.clear_convergence_state()`. Add `let mut consecutive_block_count: usize = 0;`, `let mut total_block_count: usize = 0;`, `let mut signal_break = false;`, `let mut total_tokens: u64 = 0;` (all reset on outer loop). After each successful `send_message()`, accumulate: `total_tokens += usage.input_tokens + usage.output_tokens`. Pass `total_tokens` to `hooks.run_stop()` at inner loop exit. Wrap sequential tool dispatch: for each tool_use → `hooks.run_pre_tool_use()` → if `Block` (error ToolResult, increment both counters, check thresholds: if `consecutive >= 3` or `total >= 10` → `threshold_tripped = true; break`) → else dispatch_tool + `consecutive_block_count = 0` → `hooks.run_post_tool_use()` → if Signal → `signal_break = true`. After for-loop: if `threshold_tripped` → `conversation.pop() + break` (reason: `block_limit_consecutive` or `block_limit_total`); else send tool_results + `tool_iterations += 1`; if `signal_break` → break (no recover). Wrap parallel path: guard/observe per-tool before spawn, `blocked_flags: Vec<bool>`, threshold check per-tool, `consecutive_block_count` resets at guard-allow time. If threshold trips mid-batch: join_all already-spawned (avoid detach), skip PostToolUse, `conversation.pop() + break`. Normal path: join_all, fill slots, PostToolUse loop (skip blocked slots), signal check. Stop hook at inner loop exit: `hooks.run_stop(reason, tool_iterations, total_tokens)` with one of 7 reason values (`end_turn`, `iteration_limit`, `api_error`, `continuation_cap`, `block_limit_consecutive`, `block_limit_total`, `convergence_signal`). `tool_iterations` NOT incremented on aborted batch (block threshold).
- [ ] **8d. Tests** — Guard block produces error ToolResult with hook's reason message. Guard timeout produces "timed out after {ms}ms" error. Guard crash (non-zero exit) produces "exited with code {n}" error. Guard invalid JSON produces "invalid JSON" error. Observe runs after block with `blocked: true` context. Observe failure logged, no effect. PostToolUse Signal sets flag. PostToolUse failure → Continue. Stop fires with all 7 reasons. Consecutive counter (threshold 3) triggers `conversation.pop() + break`. Total counter (threshold 10) triggers same. Consecutive resets on successful dispatch. Total never resets within inner loop. Both reset on outer loop. Convergence JSON structure (observations array + final key). Atomic write (temp + rename). Result truncation at 5KB. No-op when unconfigured (empty hooks vec). `match_tool` exact string match. `phase` None → guard for PreToolUse. Block threshold precedence over signal_break.

## 9. Release Workflow: `release-workflow.md`

Depends on: item 1 (needs `cargo build` working). Independent of items 2-8. Can be implemented anytime after item 1.

- [ ] **9a. `.github/workflows/release.yml`** — Tag-triggered (`v*`). Build matrix: `aarch64-apple-darwin` on `macos-latest`, `x86_64-unknown-linux-gnu` on `ubuntu-latest`. Per leg: `cargo test` then `cargo build --release`. Tarball: `forgeflare-{tag}-{target}.tar.gz` (binary only, gzip). Release job: `gh release create $TAG --generate-notes` with both tarballs attached. Pinned action SHAs. `jdx/mise-action` for tool orchestration. `Swatinem/rust-cache` for build caching. Permissions: `contents: read` on build jobs, `contents: write` on release job. Release job `needs: [build]` (or inline CI gate per leg).

## Missing Elements (confirmed absent)

Verified 2026-02-14: No missing specs identified. All 10 specs cover the complete feature set described in the ULTIMATE GOAL:

- Hook dispatch (PreToolUse guard/observe, PostToolUse signal, Stop finalization): `hooks.md`
- API retry with exponential backoff: `api-retry.md`
- Session transcript persistence: `session-capture.md`
- MaxTokens continuation: `maxtoken-continuation.md`
- Token-aware context trimming: `token-aware-trim.md`
- Parallel dispatch for pure tools: `tool-parallelism.md`
- Foundation (CLI, streaming, 5 tools): `coding-agent.md` + `tool-name-compliance.md`
- Configurable API endpoint: `api-endpoint.md`
- Release workflow: `release-workflow.md`

The `reference/go-source/` directory referenced by `coding-agent.md` does not exist but is not required — it was a pattern reference for the Go workshop, and the specs contain sufficient implementation detail.

## Spec Errata (fix during implementation)

- `release-workflow.md` line 77: success criteria says "working `agent` binary" — should say `forgeflare`. The plan (item 9a) and spec R3 correctly use `forgeflare`.
- `session-capture.md` JSONL example (line 101): uses snake_case `read_file` in tool_use name. Should be PascalCase `Read` per `tool-name-compliance.md`. Cosmetic — the example illustrates format, not content.

## Notes

- Items 1-2 are the critical path — nothing else can start until the binary compiles and talks to the API.
- Items 3-7 are the inner loop enhancements — each modifies `main.rs` control flow incrementally. They must be implemented in order to avoid structural conflicts.
- Item 8 (hooks) depends on the final shape of both sequential and parallel dispatch paths from items 1-7.
- Item 9 (release) is independent of Rust code and can be done anytime after `cargo build` works (item 1).
- The `<950 production lines` constraint in `coding-agent.md` applies to the foundation (item 1). Later specs add code beyond this budget, which is expected — the constraint is about keeping the core tight, not limiting the full feature set.
- The `tools!` macro in item 1d must use PascalCase names from day one (per `tool-name-compliance.md`). There is no separate rename step — the foundation ships with `Read`, `Glob`, `Bash`, `Edit`, `Grep`.
- `send_message()` return type evolves: item 1 returns `(Vec<ContentBlock>, StopReason)`, item 4 changes to `(Vec<ContentBlock>, StopReason, Usage)`. All downstream code (items 5-8) uses the three-element tuple.

## Learnings

- `futures_util::stream::once` produces non-Unpin streams; use `stream::iter` for test mocks
- Test SSE data must have proper JSON escaping — `partial_json` values need complete JSON (including closing braces)
- Grep tests searching "." will match their own source code — use temp directories for no-match assertions
- `std::io::IsTerminal` trait (Rust 1.70+) replaces FFI `isatty` calls
- The `bytes` crate must be an explicit dependency even though reqwest re-exports it
- reqwest needs the `json` feature for `.json()` request builder method
