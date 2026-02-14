# Implementation Plan

All 9 items complete. 119 tests pass, clippy clean, fmt clean. Binary compiles, all 5 tools work with PascalCase names.

Updated 2026-02-14: Fixed convergence JSON key from "final_state" to "final" per hooks.md spec (serde rename was missing). Added regression test asserting raw JSON key name.

## Completed Items

1. Foundation: `coding-agent.md` + `tool-name-compliance.md` — CLI, streaming API, 5 PascalCase tools
2. API Endpoint Configuration: `api-endpoint.md` — configurable URL, optional API key
3. API Retry: `api-retry.md` — exponential backoff, error classification
4. Session Capture: `session-capture.md` — JSONL transcripts, usage parsing
5. MaxTokens Continuation: `maxtoken-continuation.md` — 3-attempt cap, classify_max_tokens
6. Token-Aware Trim: `token-aware-trim.md` — 120K threshold gating
7. Tool Parallelism: `tool-parallelism.md` — ToolEffect enum, batch classification
8. Hook Dispatch: `hooks.md` — guard/observe/post/stop, fail-closed guards, convergence writes
9. Release Workflow: `release-workflow.md` — tag-triggered, macOS aarch64 + Linux x86_64, pinned SHAs

## Spec Errata (documented during implementation)

- `release-workflow.md` line 77: success criteria says "working `agent` binary" — should say `forgeflare`. The workflow correctly uses `forgeflare`.
- `session-capture.md` JSONL example (line 101): uses snake_case `read_file` in tool_use name. Should be PascalCase `Read` per `tool-name-compliance.md`. Cosmetic only.

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
