# MaxTokens Continuation

**Status:** Active
**Target:** Recover from MaxTokens truncation by prompting the LLM to continue instead of breaking the inner loop

---

## Why

When `stop_reason == MaxTokens`, the LLM's response was cut mid-generation. Forgeflare currently filters corrupt tool_use blocks and breaks the inner loop (the `if stop_reason != StopReason::ToolUse` block). The agent's turn is over — whatever it was reasoning about or generating is lost. The user (or Ralph loop) sees a truncated response and has to start over.

For interactive use, this is acceptable — the user can say "continue." For a Ralph loop, it's an iteration wasted. The fix is obvious: when MaxTokens fires and the truncation happened in a text block (not mid-tool-call), inject a continuation prompt and loop back to the API. This recovers mid-thought truncation without operator intervention.

---

## Requirements

**R1. Detect Continuable Truncation**
- After MaxTokens handling (filter null-input tool_use blocks, inject placeholder if empty): check if the remaining content is text-only (no pending tool_use blocks with valid inputs)
- If text-only: the LLM was mid-thought and can be prompted to continue
- If tool_use blocks remain (with valid inputs): the LLM requested tools but the last one got cut off. The valid tool_use blocks have been committed to conversation history and must be executed. The canonical control flow restructure (see Architecture section) replaces the existing `if stop_reason != ToolUse { break; }` block entirely, allowing MaxTokens with valid tools to fall through to tool dispatch naturally.

**R2. Inject Continuation Prompt**
- When continuable: push a User message with text "Continue from where you left off." to the conversation AND log it via `session.append_user_turn(conversation.last().unwrap())` — same pattern as every other User message push in the codebase
- Do NOT increment `tool_iterations` (this is not a tool dispatch, it's a response continuation)
- If the response is empty after MaxTokens handling (only the injected placeholder text, no real content), do NOT attempt continuation — break immediately. An empty MaxTokens response indicates an API-level issue, not a continuable thought.
- Loop back to the API call (continue the inner loop)

**R3. Cap Continuations**
- Maximum 3 continuation attempts per inner loop session
- Track via a `continuation_count` counter alongside `tool_iterations`
- After 3 continuations, fall through to the existing break (prevents infinite continuation loops if the LLM keeps generating to max_tokens)

**R4. Logging**
- Log each continuation: `[continue] Response truncated at max_tokens, requesting continuation ({n}/3)`
- Log when cap is reached: `[continue] Max continuations reached, breaking`

---

## Architecture

```text
Canonical control flow restructure (replaces the current `if stop_reason != ToolUse { ... break; }` block):

  // 1. Break on EndTurn — normal completion, no further dispatch
  if stop_reason == StopReason::EndTurn {
      break;
  }

  // 2. Handle MaxTokens — filter, then decide: continue, dispatch tools, or break
  if stop_reason == StopReason::MaxTokens {
      // filter null-input tool_use blocks (existing code, unchanged)
      // inject placeholder if empty (existing code, unchanged)

      // Check for empty response AFTER filtering — if only the placeholder remains,
      // this is an API-level issue, not a continuable thought
      let is_empty = /* only placeholder text, no real content blocks */;
      if is_empty {
          break;
      }

      // Check for valid tool_use blocks AFTER the null-input filter has run
      // (conversation.last() already has the filtered content via last_mut() retain)
      let has_valid_tools = conversation.last().unwrap().content.iter()
          .any(|b| matches!(b, ContentBlock::ToolUse { .. }));

      if has_valid_tools {
          // Valid tool_use blocks survived filtering — fall through to tool dispatch below.
          // Do NOT increment continuation_count (tools are being dispatched, not continued).
      } else if continuation_count < 3 {
          // Text-only truncation — inject continuation prompt
          continuation_count += 1;
          // push User("Continue from where you left off.")
          // session.append_user_turn(...)
          // log "[continue] ..."
          continue; // loop back to API call
      } else {
          // Cap reached
          // log "[continue] Max continuations reached, breaking"
          break;
      }
  }

  // 3. Tool dispatch runs for both ToolUse and MaxTokens-with-valid-tools
  // (existing tool dispatch code follows here)
```

This is the ONE canonical restructure. Do not implement it differently. The key insight: `EndTurn` breaks immediately, `MaxTokens` branches three ways (tools → dispatch, text + under cap → continue, text + cap reached → break), and `ToolUse` falls through to dispatch as before.

Changes to existing code:

1. `main.rs` — Add `let mut continuation_count: usize = 0;` alongside the `tool_iterations` declaration. Replace the `if stop_reason != StopReason::ToolUse { ... break; }` block with the canonical control flow above. The existing MaxTokens filtering code (null-input removal, placeholder injection) moves into the `if stop_reason == MaxTokens` branch.

---

## Success Criteria

- [ ] Text-only MaxTokens response triggers continuation prompt
- [ ] Tool_use MaxTokens response falls through to tool dispatch (no continuation)
- [ ] Continuation that itself hits MaxTokens correctly increments `continuation_count` toward the cap
- [ ] Empty MaxTokens response (only placeholder) does NOT trigger continuation — breaks immediately
- [ ] `continuation_count` resets on new user input (outer loop iteration)
- [ ] Continuation prompt logged to JSONL transcript via `session.append_user_turn()`
- [ ] Continuation capped at 3 attempts
- [ ] `continuation_count` does not increment `tool_iterations`
- [ ] `continuation_count` does NOT increment when MaxTokens with valid tools falls through to dispatch
- [ ] `continuation_count` does NOT reset on tool_result User messages, only on outer loop user input
- [ ] Continuation prompt appears in conversation history and JSONL transcript
- [ ] Existing MaxTokens filter tests still pass (null-input tool_use filtering unchanged)

---

## Non-Goals

- Increasing `max_tokens` dynamically (the cap is set for cost control, not as a continuation hint)
- Merging continued text blocks (the LLM naturally continues from context; separate assistant messages are fine)
- Continuation for tool_use truncation (partial tool calls can't be completed by "continue" — they need re-generation, which the existing recovery handles)
- Configurable continuation cap via CLI (3 is the right number; higher risks cost blowout, lower loses the benefit)

---

## Implementation Notes

- The continuation User message must be a plain text message, not a tool_result. This is important because `recover_conversation()` checks whether the last User message is a tool_result to decide what to pop. A text continuation message will be correctly popped as a simple User message if the next API call fails.
- The assistant's truncated message is already in the conversation (pushed at line 266-269). The continuation prompt adds a new User message after it. The LLM sees: assistant's partial text → user saying "continue" → and generates the next assistant message continuing from context. Standard turn-taking.
- When the LLM continues, it might emit a new text block OR decide it actually wants to call a tool now. Both are valid outcomes — the continuation just gives the LLM another chance to finish its thought.
- The `continuation_count` resets with each new user input (outer loop iteration). It's scoped to the inner tool loop, same as `tool_iterations`. It does NOT reset on tool_result User messages pushed during tool dispatch within the inner loop. It persists across tool dispatch cycles within the same inner loop session.
- When `tool_iterations` hits MAX_TOOL_ITERATIONS (50), the existing break and `recover_conversation()` handle cleanup regardless of continuation state. No special handling needed for `continuation_count` at the tool_iterations limit.
- Semantically truncated tool inputs: when MaxTokens fires with valid tool_use blocks, the last tool's JSON input may be syntactically valid but semantically incomplete (e.g., `{"command": "cargo test --featu"}`). The null-input filter won't catch this. This is an accepted risk — the tool will execute with the truncated input, fail, and return an error that the LLM can recover from on the next iteration. Do not add deeper input validation.
- Cost implication: the original API call plus 3 continuations can generate up to 64K output tokens total (4 * 16K). This is intentional — the alternative is a wasted Ralph iteration which costs a full fresh context window.
