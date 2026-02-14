# Token-Aware Context Trimming

**Status:** Active
**Target:** Use actual API token counts to inform context trim aggressiveness instead of relying solely on byte-size heuristics

---

## Why

`trim_conversation()` runs before every API call, serializing the entire conversation to JSON to measure byte size against a 720KB budget. This is a proxy: 720KB ≈ 180K tokens at ~4 bytes/token. The approximation is conservative by design, but it's also wasteful — it trims earlier than necessary, discarding context the model could still use.

Meanwhile, the Anthropic API returns exact token counts in every response: `usage.input_tokens` tells you precisely how many tokens the conversation consumed. This data is already parsed (in the `message_start` event handler in `SseParser::process_line`) and recorded to the JSONL transcript. It's not used for trim decisions.

The fix: track `input_tokens` across the inner loop. When the actual count approaches the model's context window, trim aggressively. When it's well under, skip the expensive re-serialization entirely. The byte heuristic stays as a safety net for the first API call (before any usage data exists).

---

## Requirements

**R1. Track Last Known Input Tokens**
- After each successful `send_message()`, store `usage.input_tokens` as `last_input_tokens: u64`
- This value reflects the actual context consumption for the most recent API call
- Initialize to 0 at the start of each outer loop iteration (new user input)

**R2. Token-Based Trim Decision**
- Before calling `trim_conversation()`, check `last_input_tokens`
- If `last_input_tokens == 0` (no token data — first API call or usage field absent): fall through to existing byte-based trim. Treat 0 as "no data available," not as a real token count.
- If `last_input_tokens > 0 && last_input_tokens < TRIM_THRESHOLD` (60% of context window = 120K tokens): skip trim entirely
- If `last_input_tokens >= TRIM_THRESHOLD`: run `trim_conversation()` with the existing byte budget

**R3. Context Window Constant**
- Define `MODEL_CONTEXT_TOKENS: u64 = 200_000` (Claude's effective context)
- Define `TRIM_THRESHOLD: u64 = MODEL_CONTEXT_TOKENS * 60 / 100` (120K tokens)
- The 60% threshold leaves 80K tokens of headroom for tool results added between API calls. A typical tool batch adds 5 results at up to 100KB each (25K tokens). 80K headroom covers even aggressive batches. Using 80% (160K) would leave only 40K headroom, which large file reads can exceed, causing a 400 context overflow before the byte trim gets a chance to run.
- These are constants, not configurable — the model context window is a known quantity

**R4. Preserve Byte-Based Safety Net**
- `trim_conversation()` itself is unchanged — same byte budget, same exchange boundary logic
- The token-based check is a gate BEFORE the byte-based trim, not a replacement
- If token tracking somehow fails (usage field missing, API changes), the byte trim still runs every time (same as today)

---

## Architecture

```text
inner loop iteration:
  │
  ├─ last_input_tokens == 0?
  │    ├─ yes → trim_conversation() (no token data yet, use byte safety net)
  │    └─ no  → last_input_tokens < TRIM_THRESHOLD (120K)?
  │                ├─ yes → skip trim (context is well under limit)
  │                └─ no  → trim_conversation(conversation, MAX_CONVERSATION_BYTES)
  │
  ├─ send_message() → Ok((response, stop_reason, usage))
  │    └─ last_input_tokens = usage.input_tokens
  │
  └─ ... (tool dispatch, etc.)
```

Changes to existing code:

1. `main.rs` — Add `let mut last_input_tokens: u64 = 0;` alongside `tool_iterations` (at the `let mut tool_iterations = 0usize;` declaration site). After the successful `send_message()` match arm, update `last_input_tokens = usage.input_tokens`. Before the `trim_conversation()` call, gate on `last_input_tokens`. If api-retry is implemented first, the `last_input_tokens` update goes after the retry loop's successful outcome, not inside the retry loop. If maxtoken-continuation is implemented, `last_input_tokens` gets updated after each successful API call regardless of whether it was a continuation, keeping the token count fresh across continuations.

---

## Success Criteria

- [ ] First API call in a session always runs byte-based trim (no token data yet, `last_input_tokens == 0`)
- [ ] Subsequent calls skip trim when `last_input_tokens > 0 && last_input_tokens < 120_000`
- [ ] Subsequent calls run trim when `last_input_tokens >= 120_000`
- [ ] Byte-based trim still fires correctly when triggered
- [ ] `last_input_tokens` resets to 0 on new user input (outer loop)
- [ ] No behavioral change when `usage.input_tokens` is 0 or absent (byte trim runs, same as today)
- [ ] Existing trim tests pass (trim logic itself unchanged)

---

## Non-Goals

- Dynamic adjustment of byte budget based on token/byte ratio (the 720KB constant is fine as a safety net)
- Token counting on the client side (tokenizer dependency is heavy and unnecessary when the API provides exact counts)
- Per-model context window configuration (all Claude models we target have 200K context)
- Aggressive trimming (trimming to a target like 60% context) — the current trim removes the minimum necessary to fit under budget, which is correct

---

## Implementation Notes

- The `last_input_tokens` variable is a single `u64`. No struct, no state object, no abstraction. It's updated in one place (after API response) and read in one place (before trim check). The `== 0` check handles both "first call" and "usage field absent" via the same code path.
- The 60% threshold (120K) is deliberately conservative. `last_input_tokens` is a lower bound on the next request's actual token count — it doesn't include the assistant's output tokens or tool results added to the conversation since the last API response. Between API calls, the conversation grows by `output_tokens + tool_result_tokens`. The 80K tokens of headroom between the 60% threshold and the 200K context window absorbs this gap. Do NOT raise the threshold to 80% — that leaves only 40K headroom, which large file reads can exceed, causing a 400 context overflow before the byte trim gets a chance to run.
- `input_tokens` includes the system prompt, tool definitions, and all conversation messages. It's the total context consumption, not just message content. This makes it the right metric for trim decisions.
- At the 60% threshold, there is 80K tokens of headroom before context overflow. If somehow tool results exceed this (e.g., multiple 1MB file reads), the byte-based trim catches it before the API call. The token check decides whether to bother running the byte trim, and the byte trim provides the hard guarantee.
- The serialization cost of `trim_conversation()` is O(conversation_size). For a 100-message conversation with large tool results, this is non-trivial. Skipping it when the token count says we're safe eliminates this overhead for the majority of inner loop iterations.
