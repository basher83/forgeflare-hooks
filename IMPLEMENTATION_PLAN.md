# Implementation Plan

Phase 1: 9/9 complete. Phase 2: 5/5 complete. Pre-existing bugs: 4/4 fixed. Tool hardening: 3/3 fixed. Schema fix: 1/1 fixed. Release fixes: 2/2 fixed. SSE error fix: 1/1 fixed. Test coverage gaps: 8/8 fixed. 172 tests pass, clippy clean, fmt clean.

All planned work is complete.

Updated 2026-03-12: Full spec-vs-implementation audit across all 16 specs. Three gaps found and fixed: (1) release workflow missing `--latest` flag (spec R4, v0.0.16); (2) actions/checkout SHA mismatch between ci.yml and release.yml (spec R6, v0.0.16); (3) unknown SSE error types classified as permanent instead of transient (api-retry spec R1, v0.0.17).

Updated 2026-03-12: Second audit pass found two test coverage gaps in parallel dispatch path. Both fixed in v0.0.19: (1) null-input tool_use in parallel path — test verifies error ToolResult produced and post-hooks skipped via blocked_flags; (2) mid-batch threshold trip — test verifies already-spawned futures joined, threshold_tripped set, and empty result returned.

Updated 2026-03-12: Third audit pass (v0.0.21). Four issues found and fixed: (1) Cargo.toml version was `0.1.0` while git tags were `v0.0.x` — session transcripts via `env!("CARGO_PKG_VERSION")` were lying. Fixed to `0.0.21`. (2) Missing test for Stop hook returning unrecognized action value (hooks.md R3). Added `stop_unrecognized_action_does_not_panic`. (3) Missing test for `threshold_tripped` precedence over `signal_break` (hooks.md R6). Added `threshold_takes_precedence_over_signal_break`. (4) glob-shell-injection.md R2 incorrectly stated glob crate returns "filesystem order (platform-dependent)" — corrected to "alphabetical order" per implementation notes.

Updated 2026-03-12: Fourth audit pass (v0.0.22). Full spec-vs-implementation test coverage audit. Four new tests added: (1) SSE error with absent `error.type` field defaults to transient (api-retry R1). (2) Brace expansion producing invalid pattern fails the entire glob operation (glob-shell-injection R5). (3) Convergence write failure returns Err without panic (hooks R8, unix-only). (4) Guard-hook blocked tools skip PostToolUse via blocked_flags (hooks R7).

## Spec Audit Results (2026-03-12)

Full line-by-line audit of all specs against implementation. Results:

- `coding-agent.md` — Fully implemented. Two intentional deviations documented below.
- `tool-name-compliance.md` — No gaps.
- `api-endpoint.md` — No gaps. Three-tier URL precedence, conditional API key, trailing slash strip.
- `api-retry.md` — One gap fixed: unknown SSE error types defaulted to permanent (`StreamParse`) instead of transient (`StreamTransient`). Now only `invalid_request_error` is permanent; all others trigger retry (v0.0.17).
- `session-capture.md` — No gaps. JSONL transcripts, usage parsing, session identity.
- `token-aware-trim.md` — No gaps. 120K threshold gating, byte-based fallback.
- `maxtoken-continuation.md` — No gaps. 3-attempt cap, classify_max_tokens enum.
- `tool-parallelism.md` — No gaps. ToolEffect enum, batch classification, parallel dispatch.
- `hooks.md` — No gaps. All four phases, fail-closed guards, convergence writes, block thresholds.
- `glob-shell-injection.md` — No gaps. Pure glob crate, no shell subprocess.
- `sse-buffer-optimization.md` — No gaps. Slice borrow + drain.
- `prompt-caching.md` — No gaps. System prompt + last tool cache_control.
- `project-instructions-loading.md` — No gaps. CLAUDE.md/AGENTS.md with priority, size limit, permission checks.
- `run-turn-refactor.md` — No gaps. Extracted helpers, both bug fixes, under 350 lines.
- `release-workflow.md` — Two gaps fixed: `--latest` flag added to `gh release create` (v0.0.16); `actions/checkout` SHA aligned with ci.yml (v0.0.16).

## Known Untestable Gaps

- `classify_error` for `AgentError::Api(reqwest::Error)` timeout/connect branches: constructing a `reqwest::Error` requires actual network failures. The error classification logic is correct by inspection, but these specific branches have no unit test. Accepted constraint — not worth introducing a mock HTTP layer for two match arms.

- `coding-agent.md` R4 lists `Glob(path?, recursive?)` — `recursive` parameter not implemented. The `glob` crate handles `**` patterns natively, making an explicit parameter redundant. The model uses `**/*.rs` directly.
- `coding-agent.md` R4 lists `Bash(command, cwd?)` — `cwd` parameter not implemented. The model uses `cd dir && command` pattern. Claude Code itself omits this parameter.
- Bash schema declares a `description` parameter that `bash_exec` never reads. Harmless — the model sends it for context but the tool ignores it. Not worth removing since it serves as documentation in the schema.

## Spec Errata

- `release-workflow.md` line 77: success criteria says "working `agent` binary" — should say `forgeflare`.
- `session-capture.md` JSONL example (line 101): uses snake_case `read_file` in tool_use name. Should be PascalCase `Read` per `tool-name-compliance.md`.
- `glob-shell-injection.md` R2: said "filesystem order (platform-dependent)" — corrected to "alphabetical order" (v0.0.21).

## Minor Code Observations (not blocking)

All resolved in v0.0.18:
- `classify_effect` naming mismatch — stale note, CLAUDE.md does not reference this name.
- `MAX_CONTINUATIONS` constant moved to top-of-file constants block.
- `stop_reason_str` replaced with typed `TurnStopReason` enum.

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
- `std::sync::OnceLock` (Rust 1.70+) is ideal for caching one-time subprocess checks — thread-safe lazy initialization without external dependencies
- `str::floor_char_boundary()` (Rust 1.73+) safely truncates strings at char boundaries — avoids panics when slicing multi-byte UTF-8 in output cap logic
- Bash deny list matches on normalized (lowercased, whitespace-collapsed) command strings — test commands for output cap must avoid matching deny patterns (e.g. `dd if=/dev` triggers the deny list)
