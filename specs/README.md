# Specs Index

| Status | Spec | Purpose | Code Location |
|--------|------|---------|---------------|
| Active | `coding-agent.md` | Unified Rust agent: single binary, streaming, 5 tools, <950 lines | `src/` |
| Active | `release-workflow.md` | Cross-platform release builds: macOS aarch64 + Linux x86_64, tag-triggered, tarballs | `.github/workflows/release.yml` |
| Active | `session-capture.md` | Persist conversation transcripts in Entire-compatible JSONL for post-session observability | `src/` (new module) + `.entire/metadata/` |
| Active | `api-endpoint.md` | Configurable API endpoint defaulting to tailnet OAuth proxy, optional API key | `src/api.rs`, `src/main.rs` |
| Active | `api-retry.md` | Retry transient API errors (429, 503, timeouts) with exponential backoff | `src/api.rs`, `src/main.rs` |
| Active | `tool-parallelism.md` | Classify tools by side effects, execute pure tools concurrently | `src/tools/mod.rs`, `src/main.rs` |
| Active | `maxtoken-continuation.md` | Continue from MaxTokens truncation instead of breaking inner loop | `src/main.rs` |
| Active | `token-aware-trim.md` | Use API token counts to skip unnecessary context trimming | `src/main.rs` |
| Active | `hooks.md` | Shell-based hook dispatch: PreToolUse guard, PostToolUse observation, Stop finalization | `src/hooks.rs` (new), `src/main.rs` |
| Complete | `tool-name-compliance.md` | Rename tool names to match Claude Code conventions for OAuth proxy compatibility | `src/tools/mod.rs`, `src/main.rs` |

## Implementation Order

Specs modify overlapping code in `main.rs` and `api.rs`. Implement in this order to avoid structural conflicts:

1. `api-endpoint.md` — changes `AnthropicClient::new` signature and CLI struct
2. `api-retry.md` — splits error variants, wraps `send_message()` in retry loop
3. `session-capture.md` — needs usage data from `send_message()` return tuple
4. `maxtoken-continuation.md` — restructures inner loop control flow (stop_reason handling)
5. `token-aware-trim.md` — reads usage after the retry loop, gates trim decisions
6. `tool-parallelism.md` — modifies the tool dispatch block (independent of control flow changes above)
7. `hooks.md` — wraps tool dispatch with hook calls (depends on final shape of both sequential and parallel paths)
