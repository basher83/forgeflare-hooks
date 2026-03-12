# Specs Index

| Status | Spec | Purpose | Code Location |
|--------|------|---------|---------------|
| Active | `glob-shell-injection.md` | Replace shell injection in glob_exec with glob crate | `src/tools/mod.rs` |
| Active | `prompt-caching.md` | Add cache_control to system prompt and tools for prompt caching | `src/api.rs` |
| Active | `project-instructions-loading.md` | Load CLAUDE.md project instructions into system prompt | `src/main.rs` |
| Active | `run-turn-refactor.md` | Extract shared dispatch logic from parallel/sequential paths | `src/main.rs` |
| Active | `sse-buffer-optimization.md` | Eliminate unnecessary String reallocation in SSE parser | `src/api.rs` |
| Complete | `coding-agent.md` | Unified Rust agent: single binary, streaming, 5 tools, <950 lines | `src/` |
| Complete | `release-workflow.md` | Cross-platform release builds: macOS aarch64 + Linux x86_64, tag-triggered, tarballs | `.github/workflows/release.yml` |
| Complete | `session-capture.md` | Persist conversation transcripts in Entire-compatible JSONL for post-session observability | `src/session.rs` + `.entire/metadata/` |
| Complete | `api-endpoint.md` | Configurable API endpoint defaulting to tailnet OAuth proxy, optional API key | `src/api.rs`, `src/main.rs` |
| Complete | `api-retry.md` | Retry transient API errors (429, 503, timeouts) with exponential backoff | `src/api.rs`, `src/main.rs` |
| Complete | `tool-parallelism.md` | Classify tools by side effects, execute pure tools concurrently | `src/tools/mod.rs`, `src/main.rs` |
| Complete | `maxtoken-continuation.md` | Continue from MaxTokens truncation instead of breaking inner loop | `src/main.rs` |
| Complete | `token-aware-trim.md` | Use API token counts to skip unnecessary context trimming | `src/main.rs` |
| Complete | `hooks.md` | Shell-based hook dispatch: PreToolUse guard, PostToolUse observation, Stop finalization | `src/hooks.rs`, `src/main.rs` |
| Complete | `tool-name-compliance.md` | Rename tool names to match Claude Code conventions for OAuth proxy compatibility | `src/tools/mod.rs`, `src/main.rs` |

## Implementation Order (Phase 2)

New specs from security/quality review. Ordered by priority and dependency:

1. `glob-shell-injection.md` — security fix, no dependencies, changes only `tools/mod.rs`
2. `sse-buffer-optimization.md` — trivial fix, no dependencies, changes only `api.rs`
3. `prompt-caching.md` — changes `send_message()` request construction in `api.rs`
4. `project-instructions-loading.md` — adds `load_project_instructions()` to `main.rs`, interacts with prompt caching (either order fine)
5. `run-turn-refactor.md` — refactors dispatch paths in `main.rs`, implement last (largest change surface)

## Implementation Order (Phase 1 — Complete)

1. `coding-agent.md` — foundation: CLI, streaming API client, 5 tools, conversation loop
2. `tool-name-compliance.md` — PascalCase tool names for OAuth proxy compatibility
3. `api-endpoint.md` — changes `AnthropicClient::new` signature and CLI struct
4. `api-retry.md` — splits error variants, wraps `send_message()` in retry loop
5. `session-capture.md` — needs usage data from `send_message()` return tuple
6. `maxtoken-continuation.md` — restructures inner loop control flow
7. `token-aware-trim.md` — reads usage after the retry loop, gates trim decisions
8. `tool-parallelism.md` — modifies the tool dispatch block
9. `hooks.md` — wraps tool dispatch with hook calls
10. `release-workflow.md` — GitHub Actions workflow
