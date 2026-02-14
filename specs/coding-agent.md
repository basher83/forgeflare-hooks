# Unified Rust Coding Agent Specification

**Status:** Active
**Target:** Single binary, streaming, subagent-aware, <950 production lines
**Pin:** Go source at `/reference/go-source/` — pattern-match against working code

---

## Requirements

**R1. Core Loop**
- REPL: read user input → call Claude API → dispatch tools → repeat
- Streaming responses (SSE, not batch) from day 1
- Conversation history management (add user input, add assistant message to context array)

**R2. HTTP Client**
- Build own with `reqwest` (not third-party Anthropic crate)
- Anthropic API: POST to `/v1/messages`
- Handle SSE for streaming responses
- Expose `ANTHROPIC_API_KEY` env var

**R3. Tool Registry Pattern**
```rust
// tools! macro generates all_tool_schemas() from one definition
tools! {
    "read_file", "Read a file", schema;
    "list_files", "List files", schema;
    // ...
}
// dispatch_tool() is hand-written — bash gets streaming callback, others don't
```
- 5 tools total; each follows Anthropic tool_use spec
- Tool schemas are sent with every API request, providing introspection natively
- Macro generates schemas; dispatch is hand-written to support per-tool signatures (bash streaming)

**R4. Five Tools**
1. **Read** — Read(path) → file contents (handle binary, size limits)
2. **Glob** — Glob(path?, recursive?) → [files]
3. **Bash** — Bash(command, cwd?) → stdout/stderr (timeout, streaming output via callback)
4. **Edit** — Edit(path, old_str, new_str, replace_all?) → success/error (exact match by default; replace_all=true for bulk changes; empty old_str on missing file = create with mkdir, empty old_str on existing file = append)
5. **Grep** — Grep(pattern, path?, file_type?, case_sensitive?) → matches (shell out to `rg`)

**R5. CLI Interface**
- Single binary, no subcommands required
- `--verbose` flag for debug output
- `--model` flag (default: claude-opus-4-6)
- `--max-tokens` flag (default: 16384, API supports up to 128K)
- Read prompts/context from stdin if available, interactive prompt otherwise
- Piped stdin read as single prompt (not line-by-line)
- Exit gracefully on EOF or "exit" command

**R6. Error Handling**
- `thiserror` for structured error types in api.rs (`AgentError`)
- Tool errors returned as `Result<String, String>` — raw strings flow into tool_result text
- Display errors to user, continue loop (don't panic)

**R7. Streaming Architecture**
- Collect SSE events into a response buffer
- Check `stop_reason` for "tool_use" vs "end_turn"
- If tool_use: extract tool calls, execute, send results back
- If end_turn: display response to user, prompt for next input

**R8. Subagent Awareness (Deferred)**
- Explicitly deferred per non-goals; no SubagentContext types in codebase
- StopReason enum is the only surviving artifact (used for tool dispatch loop control)
- Future subagent dispatch would add a StopReason variant or context field

---

## Architecture

```
src/
  main.rs         — CLI, loop, error handling
  api.rs          — Anthropic client (reqwest + SSE)
  tools/
    mod.rs        — All 5 tools with tools! macro (read, list, bash, edit, search)

reference/
  go-source/      — Cloned Go workshop code (pin)
```

---

## Success Criteria

- [x] Binary compiles (`cargo build`)
- [x] Tests pass (`cargo test`)
- [x] Clippy clean (`cargo clippy -- -D warnings`)
- [x] Formatted (`cargo fmt --check`)
- [x] Can chat with Claude
- [x] Can read files
- [x] Can list directories
- [x] Can run bash commands
- [x] Can edit files (exact-match semantics)
- [x] Can search code
- [x] <950 production lines (~900 actual after streaming + replace_all)
- [x] Streaming responses visible to user in real-time

---

## Dependencies

- `reqwest` — HTTP client (with stream feature)
- `serde` + `serde_json` — JSON serialization
- `tokio` — async runtime (full features)
- `clap` — CLI parsing (derive feature)
- `thiserror` — error types
- `futures-util` — stream consumption for SSE parsing

---

## Non-Goals

- Progressive binaries (Phases 1-3 merged into single unified agent)
- Batch mode (streaming from day 1)
- Provider abstraction (Anthropic only)
- Interactive line editing (simple readline via stdin)
- Subagent execution (StopReason enum retained for loop control, SubagentContext removed)

---

## Implementation Notes

- `edit_file` uses exact single-match by default; `replace_all=true` replaces every occurrence (for renames, bulk changes)
- Tool dispatch is synchronous; async only for HTTP and command execution
- Context accumulates in memory; no persistence layer. Conversation trimmed at exchange boundaries (720KB budget, ~180K tokens) to prevent unbounded growth while preserving tool_use/tool_result pairing
- No automatic retry; failures return to user for decision
- Search tool shells out to `rg` (must be installed)
- Dynamic system prompt: `build_system_prompt()` injects cwd, platform, structured tool guidance, and safety rules at startup
- reqwest client timeouts: 30s connect, 300s request (prevents indefinite hangs)
- Bash command guard: deny-list blocks destructive patterns (rm -rf /, rm -fr /, fork bombs, dd to devices, mkfs, chmod 777 /, git push --force, git push -f) before shell execution, including reversed flag order variants. Commands are whitespace-normalized (lowercase + collapse spaces/tabs) before matching to catch bypass via extra whitespace
- NO_COLOR convention: all ANSI output suppressed when `NO_COLOR` env var is set
- API error recovery: pop trailing User message + orphaned tool_use to maintain conversation alternation invariant
- Tool loop safety: 50-iteration limit prevents runaway agent behavior; calls recover_conversation on break to maintain alternation invariant
- Tool result visibility: non-verbose mode shows result size (chars); errors always shown with 200-char preview (matches Go reference pattern of always showing tool results)
- Tool schema descriptions enriched with limits (1MB, 100KB, 1000 entries, 50 matches, 120s timeout) so the model sees constraints in both schema and system prompt
- Retry-After header surfaced on 429 rate limit responses for better user-facing diagnostics
- code_search surfaces actionable error when rg (ripgrep) is not installed instead of cryptic "No such file" OS error
- Bash streaming: channel-based output streaming via mpsc channels + 50ms polling loop. Reader threads send 4KB chunks, polling loop drains and forwards to caller callback. Partial output preserved on timeout. Eliminates wait-timeout dependency (replaced by try_wait + Instant deadline)
- edit_file replace_all: optional boolean parameter for bulk replacements. Default false preserves single-match safety. Error message hints at replace_all when duplicates found

---

## Reference: Go Source

The Go workshop (`reference/go-source/`) contains 6 progressive versions:
- `chat.go` — bare event loop
- `read.go` — +read_file tool
- `list_files.go` — +list_files tool
- `bash_tool.go` — +bash tool
- `edit_tool.go` — +edit_file tool
- `code_search_tool.go` — +code_search tool

Study the event loop in `edit_tool.go` (lines 126-214) as the canonical loop pattern. The Rust implementation should follow the same structure: API call → check response → dispatch tools → send results → repeat.
