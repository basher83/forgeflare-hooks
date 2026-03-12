---
status: Active
created: 2026-03-11
---

# Prompt Caching for System Prompt and Tools

**Target:** Add `cache_control` to system prompt and tool definitions to enable Anthropic's prompt caching, reducing input token costs by up to 90% on multi-turn conversations

---

## Why

Every API call in `send_message()` re-sends the full system prompt and all tool definitions at full token cost. For a 50-iteration Ralph loop where the system prompt and tools consume ~3K tokens each call, that's ~150K wasted input tokens that could be served from cache at 10% cost after the first call.

The infrastructure is half-built: the `Usage` struct already tracks `cache_creation_input_tokens` and `cache_read_input_tokens` from the API response, and the SSE parser correctly extracts these values from `message_start` events. The request side just never asks for caching.

Anthropic's prompt caching requires sending the system prompt as an array of content blocks (not a plain string) with `cache_control: {"type": "ephemeral"}` on the last block. Similarly, tool definitions need `cache_control` on the last tool in the array. The API then caches everything up to and including the marked blocks.

---

## Requirements

**R1. System Prompt as Cached Content Block**
- Change the `"system"` field in the API request body from a plain string to an array of content blocks
- The array contains a single text block with the system prompt content
- The text block includes `"cache_control": {"type": "ephemeral"}`
- Format:

```json
"system": [
  {
    "type": "text",
    "text": "You are a coding assistant...",
    "cache_control": {"type": "ephemeral"}
  }
]
```

**R2. Tool Definitions with Cache Control**
- Add `"cache_control": {"type": "ephemeral"}` to the LAST tool definition in the tools array
- Only the last tool needs the marker — the API caches everything up to and including it
- Do not modify tool schemas in `tools/mod.rs`. Add the cache_control field in `send_message()` when constructing the request body, after cloning the tools array.

**R3. No Change to Response Parsing**
- The `Usage` struct and SSE parser already handle cache token fields correctly
- No changes needed to `parse_sse_stream` or `Usage`

**R4. Verbose Logging**
- In verbose mode, after each API response, log cache hit rate: `[verbose] Cache: {cache_read_input_tokens} read, {cache_creation_input_tokens} created, {input_tokens} total input`
- This lets operators verify caching is working

---

## Architecture

```text
send_message()
  │
  ├─ Construct system as array of content blocks (not string)
  │    └─ Single text block with cache_control
  │
  ├─ Clone tools array, add cache_control to last element
  │
  ├─ Build request body with modified system and tools
  │
  └─ Send request (rest unchanged)
```

Changes to existing code:

1. `src/api.rs`, `send_message()` — Replace `"system": system` on line 143 of the `json!` macro with the content block array inline. Replace the existing `if !tools.is_empty()` block (lines 148-150) with the clone-and-mutate version that adds cache_control to the last tool.
2. `src/main.rs`, `run_turn()` — Add cache hit logging on the common path after usage destructuring (~line 418), before block processing. This runs for every API response regardless of stop reason. Guard with `if cli.verbose`.

---

## Success Criteria

- [ ] System prompt sent as array of content blocks with `cache_control`
- [ ] Last tool definition includes `cache_control`
- [ ] Second API call in a conversation shows non-zero `cache_read_input_tokens`
- [ ] First API call shows non-zero `cache_creation_input_tokens`
- [ ] Verbose mode logs cache statistics per API call
- [ ] All existing tests pass (SSE parser tests don't depend on request format)
- [ ] Tool schemas from `all_tool_schemas()` remain unmodified (cache_control added at send time)

---

## Non-Goals

- Caching conversation messages (only system and tools are cacheable with the ephemeral strategy)
- Cache TTL configuration (Anthropic controls TTL server-side, currently 5 minutes)
- Conditional caching based on conversation length (always cache — the overhead is negligible)
- Beta header management (prompt caching is GA, no beta header needed)

---

## Implementation Notes

- The `anthropic-version` header is already `2023-06-01`. Prompt caching works with this version. No header changes needed.
- The `send_message` function continues to accept the system prompt as `&str`. The string-to-content-block conversion happens inside `send_message()`, not at the call site. This preserves the interface for callers (including the project-instructions-loading spec, which appends to the system prompt string before passing it in).
- Construct the system prompt as a content block array inline in the `json!` macro (replace line 143, do not add a separate reassignment after the macro):

```rust
let mut body = serde_json::json!({
    "model": model,
    "max_tokens": max_tokens,
    "system": [{
        "type": "text",
        "text": system,
        "cache_control": {"type": "ephemeral"}
    }],
    "messages": messages,
    "stream": true,
});
```

- Replace the existing `if !tools.is_empty()` block (lines 148-150) with clone-and-mutate:

```rust
if !tools.is_empty() {
    let mut cached_tools = tools.to_vec();
    if let Some(last) = cached_tools.last_mut() {
        last["cache_control"] = serde_json::json!({"type": "ephemeral"});
    }
    body["tools"] = serde_json::Value::Array(cached_tools);
}
```

- Verbose cache logging in `run_turn` (common path, ~line 418, after usage destructuring):

```rust
if cli.verbose {
    eprintln!(
        "[verbose] Cache: {} read, {} created, {} total input",
        usage.cache_read_input_tokens, usage.cache_creation_input_tokens, usage.input_tokens
    );
}
```

- The existing SSE parser test `parse_sse_usage_from_message_start_and_delta` already tests cache token parsing. No new parser tests needed.
- Cache creation costs 25% more than regular input tokens on the first call, but subsequent calls read from cache at 10% cost. Break-even is at 2 API calls per session. Ralph loops typically make 10-50 calls per iteration, so the ROI is massive.
