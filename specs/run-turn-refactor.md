---
status: Active
created: 2026-03-11
---

# Extract Shared Tool Dispatch Logic from run_turn

**Target:** Reduce `run_turn` from ~480 lines to ~300 by extracting the duplicated pre-hook, dispatch, post-hook, and threshold logic shared between parallel and sequential tool dispatch paths

---

## Why

`run_turn` in `src/main.rs` is 480 lines with 8 parameters (already suppressed with `#[allow(clippy::too_many_arguments)]`). The parallel dispatch path (lines ~505-662) and sequential dispatch path (lines ~663-747) share substantial structural duplication:

- Null-input guard: check `input.is_null()`, produce error ToolResult
- Pre-hook dispatch: call `hooks.run_pre_tool_use()`, handle Block vs Allow
- Block counting: increment `consecutive_block_count` and `total_block_count`, check thresholds
- Post-hook dispatch: call `hooks.run_post_tool_use()`, check for signal
- Result formatting: `format_tool_result_display()` with verbose/error branching

Both paths implement the same protocol around the actual tool execution — they differ only in HOW tools are dispatched (spawn_blocking + join_all vs sequential for-loop). The protocol logic is ~100 lines duplicated across both branches.

This duplication was introduced by the tool-parallelism spec, which correctly identified that the parallel and sequential paths need the same pre/post behavior but implemented it by copying the sequential path's logic into the parallel branch. The right fix is extracting the shared protocol into reusable pieces.

---

## Requirements

**R1. Extract Pre-Dispatch Protocol**
- Create an async function that encapsulates: null-input check, pre-hook invocation, block counting, threshold checking
- Returns a decision enum: `Allow`, `Blocked(ContentBlock)`, or `ThresholdTripped`
- Null-input handling is unified to the parallel path's behavior: produce a `ToolResult` with `"null input (truncated tool_use)"` and `is_error: true`, returned as `Blocked(ContentBlock)`. The caller treats all `Blocked` returns identically — including setting `blocked_flags` so post-hooks are skipped. The sequential path currently silently skips null-input tools; the parallel path produces the error ToolResult but fails to set `blocked_flags`, causing post-hooks to fire on fabricated results. Both are fixed by this unification.
- Block counting uses `&mut` parameters for `consecutive_block_count` and `total_block_count`. Do not return increments — `&mut` makes it impossible for the caller to forget to apply them.
- `hooks.run_pre_tool_use()` is async, so this function must be `async fn`
- Both parallel and sequential paths call this function identically

**R2. Extract Post-Dispatch Protocol**
- Create a function that encapsulates: result formatting/logging, post-hook invocation, signal detection
- `hooks.run_post_tool_use()` is async, so this function must be `async fn`
- Signature: `async fn run_post_dispatch(hooks, name, input, content: &str, is_error: bool, iterations, verbose) -> bool`
- The function takes raw result data (`content` and `is_error`), not a ContentBlock reference. This keeps it decoupled from the API wire format. The parallel path destructures the ContentBlock from its slot before calling; the sequential path passes the raw dispatch output directly.
- In the sequential path, the ownership sequence is: (1) dispatch_tool returns `Result<String, String>`, (2) match to get `(content, is_error)`, (3) call `run_post_dispatch` borrowing `&content`, (4) THEN build ContentBlock moving `content` into it. The borrow in step 3 must end before the move in step 4.
- In the parallel path, the ContentBlock already exists in the slot. Destructure it to get `&content` and `is_error`, call `run_post_dispatch`, done.
- The function does NOT produce a new ContentBlock — it observes the result for formatting/logging and runs the post-hook, returning only the signal_break flag.
- The dispatch step itself is NOT extracted — parallel and sequential paths have fundamentally different dispatch mechanics (no-op callback vs. verbose streaming callback). The extraction covers only the protocol around dispatch.

**R3. Preserve Behavior (with one intentional change)**
- The refactored code must produce identical results for:
  - Pure tool batches (parallel dispatch)
  - Mutating tool batches (sequential dispatch)
  - Mixed batches (sequential dispatch)
  - Blocked tools (guard hook rejects)
  - Threshold trips (consecutive or total block limits)
  - Signal breaks (post-hook convergence signal)
- Intentional changes:
  - Null-input tool_use blocks now produce an error ToolResult in both paths (sequential previously skipped silently)
  - Null-input tools now set `blocked_flags` in the parallel path, preventing post-hooks from firing on them (fixes pre-existing bug where post-hooks received fabricated error results for tools that never executed)

**R4. Reduce run_turn Line Count**
- Target: `run_turn` should be under 350 lines after extraction
- The extracted functions live in `main.rs` (private module-level functions, not a new module)
- `run_turn` itself should read as: classify batch, for each tool call protocol, dispatch (parallel or sequential), collect results, handle threshold/signal breaks

**R5. Maintain clippy::too_many_arguments Suppression**
- The 8-parameter signature of `run_turn` is NOT part of this refactor
- Reducing parameters would require introducing a context struct, which is a larger architectural change
- Keep the `#[allow(clippy::too_many_arguments)]` annotation

---

## Architecture

```text
Pre-dispatch (shared, async):
  run_pre_dispatch(hooks, id, name, input, iterations, &mut consecutive, &mut total)
    → PreDispatchResult { Allow, Blocked(ContentBlock), ThresholdTripped }
    (null-input check is internal — returns Blocked with error ToolResult)

Post-dispatch (shared, async):
  run_post_dispatch(hooks, name, input, content: &str, is_error: bool, iterations, verbose)
    → bool  // signal_break

Parallel path (run_turn):
  ├─ for each tool_use:                         // pre-dispatch loop
  │    ├─ run_pre_dispatch.await → match:
  │    │    ├─ Allow → spawn_blocking(dispatch_tool) into futures vec
  │    │    ├─ Blocked(cb) → store cb in slot, mark blocked
  │    │    └─ ThresholdTripped → break (join already-spawned futures, abandon batch)
  ├─ join_all(futures)                           // dispatch
  ├─ for each (result, blocked_flag):            // post-dispatch loop
  │    └─ if !blocked: run_post_dispatch.await → track signal
  └─ assemble tool_results message

Sequential path (run_turn):
  ├─ for each tool_use:                          // single interleaved loop
  │    ├─ run_pre_dispatch.await → match:
  │    │    ├─ Allow → dispatch_tool (verbose streaming callback), run_post_dispatch.await
  │    │    ├─ Blocked(cb) → collect cb
  │    │    └─ ThresholdTripped → break
  └─ assemble tool_results message
```

Changes to existing code:

1. `src/main.rs` — Extract 2-3 private functions from the parallel and sequential branches. Rewrite both branches to call the shared functions. No new modules, no new files.

---

## Success Criteria

- [ ] All 119 existing tests pass with zero modification
- [ ] `cargo clippy -- -D warnings` clean
- [ ] `run_turn` is under 350 lines
- [ ] No duplicated pre-hook, post-hook, or threshold logic between parallel and sequential paths
- [ ] Parallel path still uses `spawn_blocking` + `join_all`
- [ ] Sequential path still dispatches tools one at a time with streaming callback
- [ ] Hook behavior identical: guard blocks, observe hooks, post signals, threshold trips
- [ ] Verbose vs non-verbose logging unchanged

---

## Non-Goals

- Reducing `run_turn` parameter count (requires context struct — separate concern)
- Splitting `run_turn` into multiple functions along the API-call/tool-dispatch/continuation boundary (would require passing mutable state between functions, not worth the complexity)
- Moving tool dispatch to a separate module (the logic is tightly coupled to conversation state)
- Changing the parallel/sequential dispatch strategy or batch classification logic
- Adding new features during refactor (pure code motion with two intentional fixes: null-input unification and blocked_flags bug fix)

---

## Implementation Notes

- Both `run_pre_dispatch` and `run_post_dispatch` must be `async fn` — `hooks.run_pre_tool_use()` and `hooks.run_post_tool_use()` are both async (they call `run_hook_subprocess(...).await`). The parallel path calls these on the main thread (before spawn / after join_all), not inside the spawned tasks, so async is fine in both paths.
- The dispatch step itself is intentionally NOT extracted. The parallel path dispatches with a no-op callback (`&mut |_: &str| {}`), while the sequential path dispatches with a verbose streaming callback that prints to stderr. This asymmetry is fundamental to the parallel/sequential split and cannot be unified without changing behavior.
- Block counting (`consecutive_block_count`, `total_block_count`) uses `&mut` parameters passed to `run_pre_dispatch`. The count persists across API turns (declared at the top of the conversation loop), not just within a batch. `&mut` makes it impossible for the caller to forget to apply increments.
- `consecutive_block_count` is reset to 0 inside `run_pre_dispatch` when the result is `Allow`. This means in a parallel batch, tool 1 (allowed) resets the count, then tool 2 (blocked) increments it. The count reflects consecutive blocks within the pre-dispatch loop, which matches the current behavior.
- The `ThresholdTripped` return from `run_pre_dispatch` does NOT mean "nothing was spawned." In the parallel path, pre-dispatch and spawn alternate in the same loop iteration. If tool 2 trips the threshold, tool 1's future may already be running. The caller must join already-spawned futures before abandoning the batch.
- The sequential path's post-hook signal does NOT break the inner tool loop in the current code — it only sets a flag that breaks the outer turn loop after tool_results are collected. The extracted `run_post_dispatch` returns `bool` for signal_break, and the sequential path must preserve this: set the flag, do NOT break the tool loop early.
- This fixes a pre-existing bug where null-input tools in the parallel path did not set `blocked_flags`, causing post-hooks to fire on fabricated error results. After extraction, all `Blocked` returns (whether from null-input or guard hooks) follow the same caller-side path including flag-setting, eliminating the bug by construction.
- Note: `filter_null_input_tool_use` (line ~421) already strips null-input ToolUse blocks from MaxTokens responses before they reach the dispatch loop. The null-input check inside `run_pre_dispatch` is therefore a safety net for the unlikely case of null-input under a ToolUse stop_reason (API anomaly), not the primary handler. Both defenses should remain.
- Test coverage is already comprehensive (119 tests). The null-input unification and blocked_flags fix are the only behavioral changes. Consider adding a test that verifies: (a) null-input produces an error ToolResult, and (b) post-hooks are NOT called for null-input tools.
- This has no dependency on other specs. It can be implemented in any order.
