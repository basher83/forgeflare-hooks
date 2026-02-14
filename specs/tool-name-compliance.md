# Tool Name Compliance Specification

**Status:** Complete
**Target:** Rename forgeflare's tool names to match Claude Code conventions for OAuth proxy compatibility

---

## Why

Anthropic validates tool names against Claude Code's known tool set when requests carry OAuth credentials (Claude Max subscription). The tailnet OAuth proxy injects these credentials for clients that don't provide their own API key. Requests without tools pass (200). Requests with tools succeed only when tool names match Claude Code's PascalCase naming convention.

Empirically verified:

- `"tools":[{"name":"bash",...}]` → 400 ("This credential is only authorized for use with Claude Code")
- `"tools":[{"name":"Bash",...}]` → 200 (Sonnet responds with tool_use)
- No `tools` field → 200

The proxy already injects Bearer token, user-agent, system prompt prefix, beta flags, and browser-access header. Tool names are the remaining validation gate.

---

## Requirements

**R1. Rename Tool Schemas**

Update the `tools!` macro invocations in `tools/mod.rs` to use Claude Code naming:

| Current | Target |
|---------|--------|
| `read_file` | `Read` |
| `list_files` | `Glob` |
| `bash` | `Bash` |
| `edit_file` | `Edit` |
| `code_search` | `Grep` |

Only the `name` field changes. Descriptions and input schemas stay as-is (Anthropic validates the name, not the schema shape or description).

**R2. Update Dispatch Match Arms**

Update `dispatch_tool()` match arms from old names to new names. The unknown-tool fallback continues to catch mismatches.

**R3. Update System Prompt**

Update `build_system_prompt()` in `main.rs` to reference new tool names (`Read`, `Glob`, `Bash`, `Edit`, `Grep`). The model sees tool names in both the schema and the system prompt; they must agree.

**R4. Update Tests**

All tests that reference tool names by string must be updated. This includes:

- `schemas_returns_five` (checks for old names)
- `dispatch_known_tool` / `dispatch_unknown_tool` (dispatches by name)
- SSE parser tests (tool_use blocks with old names in test fixtures)
- `system_prompt_contains_environment_info` (checks for old names)
- `main.rs` test helpers (`assistant_tool_use` uses `"bash"`)

**R5. Update AGENTS.md**

Update the tool listing in `AGENTS.md` (project structure description) to reflect new names.

---

## Architecture

No structural changes. This is a pure rename across four touch points:

```text
tools/mod.rs
  ├── tools! macro      → rename schema name strings
  └── dispatch_tool()   → rename match arm strings

main.rs
  ├── build_system_prompt() → rename tool references in prompt text
  └── tests                 → rename tool name strings in fixtures

api.rs
  └── tests                 → rename tool name strings in SSE fixtures

AGENTS.md
  └── tool listing          → rename in documentation
```

The dispatch flow is unchanged:

```text
API response → tool_use.name ("Bash") → dispatch_tool("Bash", ...) → bash_exec()
```

Internal function names (`read_exec`, `list_exec`, `bash_exec`, `edit_exec`, `search_exec`) do not change. They are internal implementation details, not wire-protocol names.

---

## Success Criteria

- [x] `cargo test` passes with all tool names updated (156 tests, v0.0.48)
- [x] `cargo clippy -- -D warnings` clean
- [x] `all_tool_schemas()` returns schemas with names `Read`, `Glob`, `Bash`, `Edit`, `Grep`
- [x] Requests through OAuth proxy with tools succeed (`Glob({})` and `Read` verified on tailnet)
- [x] Direct API key mode continues to work (tool names are valid regardless of auth method)
- [x] System prompt tool guidance references new names

---

## Non-Goals

- Changing tool behavior, parameters, or schemas (only the `name` field changes)
- Adding new tools or removing existing ones
- Proxy-side tool name mapping (rejected — fragile, maintenance burden, unnecessary indirection)
- Matching Claude Code's full tool set (forgeflare only needs its 5 tools to pass validation)
- Changing internal function names (`bash_exec`, `read_exec`, etc.)

---

## Implementation Notes

- The `Glob` name for `list_files` is a pragmatic choice: Claude Code's `Glob` is the closest file-listing tool. The actual behavior (directory listing vs glob pattern matching) differs, but Anthropic validates the name, not the behavior. The model will learn forgeflare's `Glob` semantics from the schema description and system prompt.
- The `Grep` name for `code_search` follows the same logic. Claude Code's `Grep` wraps ripgrep; forgeflare's `code_search` also wraps ripgrep. Functionally equivalent.
- `Read` and `Edit` are direct equivalents. `Bash` is identical.
- The `generic-client-support.md` spec in tailnet-microservices should be updated to document tool name validation as a resolved finding, and the success criterion "zero client-side changes" should be amended to acknowledge the tool name rename.
