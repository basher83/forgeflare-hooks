# Parallel Dispatch for Pure Tools

**Status:** Active
**Target:** Classify tools by side effects and execute read-only tools concurrently within a single tool batch

---

## Why

When Claude requests three Read calls in one response, forgeflare dispatches them sequentially (the `for block in ...` tool dispatch loop in the inner loop). Each waits for the previous to complete before starting. Read, Glob, and Grep are pure — no side effects, no ordering dependency, no shared state. Running them concurrently cuts wall-clock time proportionally to the batch size.

This matters for Ralph loops doing heavy codebase exploration. A typical iteration's orient phase requests 3-5 parallel Reads or Greps. At ~50ms per file read, sequential dispatch adds 150-250ms per orient. Over 50 tool iterations, that's measurable. More importantly, concurrent dispatch reduces the time the API connection sits idle waiting for tool results, keeping the iteration tight.

The prerequisite is knowing which tools are safe to parallelize. That requires a classification: `{Read, Glob, Grep}` are pure (no side effects), `{Bash, Edit}` are mutating (modify filesystem or execute arbitrary commands). This classification is also useful for future work (smarter recovery logic, post-tool hooks) but the immediate use is parallelism.

---

## Requirements

**R1. Tool Effect Classification**
- Define a `pub` `ToolEffect` enum: `Pure` and `Mutating`
- Map each tool: `Read → Pure`, `Glob → Pure`, `Grep → Pure`, `Bash → Mutating`, `Edit → Mutating`
- Expose via a `pub fn tool_effect(name: &str) -> ToolEffect` in `tools/mod.rs`
- Unknown tool names default to `Mutating` (safe default)
- Import in `main.rs`: update the existing `use tools::{all_tool_schemas, dispatch_tool};` to include `tool_effect` and `ToolEffect`

**R2. Batch Classification**
- Before dispatching tools, classify the entire batch
- If ALL tools in the batch are `Pure`: dispatch concurrently
- If ANY tool in the batch is `Mutating`: dispatch entire batch sequentially (existing behavior)
- This is conservative — a mixed batch with 4 Reads and 1 Edit runs everything sequentially. That's correct: the Edit might depend on a Read result, and the LLM may have ordered them intentionally.

**R3. Concurrent Dispatch**
- Use `futures_util::future::join_all()` to await all pure tool calls concurrently (the project depends on `futures-util`, NOT the `futures` umbrella crate — import from `futures_util`)
- The parallel path MUST replicate the null-input guard from the sequential path (check `input.is_null()` and produce an error ToolResult). Recommended approach: check inside each `spawn_blocking` closure, which preserves position ordering naturally since every block gets a future. Do NOT filter before spawning — that creates an ordering problem where null-input error results need to be re-merged at the correct positions.
- Log each tool name BEFORE spawning (same `tool: Name` format as the sequential path) so the user sees progress during concurrent execution
- Collect results in the same order as the original tool_use blocks (preserve ordering for conversation coherence)
- Error handling per-tool: a failed Read doesn't cancel other concurrent Reads. Each produces its own ToolResult (success or error).

**R4. Preserve Sequential Fallback**
- The `Mutating` path is exactly the existing `for block in ...` tool dispatch loop
- No behavioral change for any batch containing Bash or Edit
- The streaming callback for Bash continues to work as-is in the sequential path

---

## Architecture

```text
tool_use blocks from assistant response
  │
  ├─ classify batch → all Pure?
  │    ├─ yes →
  │    │    ├─ for each tool: log "tool: Name", check null-input guard
  │    │    ├─ spawn_blocking(dispatch_tool) for each valid tool
  │    │    ├─ join_all → collect Vec<(String, ContentBlock)> in order (name + result)
  │    │    └─ log results using preserved names
  │    └─ no  → sequential for-loop (existing code, unchanged)
  │
  ├─ tool_iterations += 1 (BOTH paths must increment)
  │
  └─ tool_results sent back as User message (unchanged)
```

Changes to existing code:

1. `tools/mod.rs` — Add `pub ToolEffect` enum and `pub fn tool_effect()` function.
2. `main.rs` — Before the `for block in ...` tool dispatch loop, classify the batch. Branch: if all pure, use `join_all`; otherwise, existing for-loop. The `join_all` path constructs futures from `dispatch_tool()` calls and awaits them together. `tool_iterations += 1` must be present after BOTH the sequential and parallel paths.

---

## Success Criteria

- [ ] 3 concurrent Read calls complete faster than 3 sequential Read calls
- [ ] Mixed batch (Read + Edit) dispatches sequentially (no parallelism)
- [ ] Tool results maintain original ordering regardless of dispatch strategy
- [ ] Individual tool errors don't cancel sibling concurrent tools
- [ ] Existing sequential dispatch tests still pass
- [ ] `ToolEffect` classification is exhaustive (every tool name mapped)
- [ ] Unknown tool names classified as `Mutating`
- [ ] `JoinError` (thread panic) produces a valid ToolResult error with correct tool_use_id
- [ ] Per-tool logging (result size, error preview) present in parallel path
- [ ] Batch of 1 pure tool works correctly (degenerate parallel case)

---

## Non-Goals

- Fine-grained parallelism within mixed batches (e.g., parallelize only the Reads, then run Edit sequentially). The conservative all-or-nothing approach is simpler and correct.
- Async tool implementations (Read/Glob/Grep are sync filesystem ops wrapped in `tokio::task::spawn_blocking`). Making them truly async would require rewriting each tool.
- Configurable concurrency limits (tool batches are typically 1-5 items; no throttling needed)
- Parallelism for Bash commands (even "safe-looking" bash commands can have side effects — always sequential)

---

## Implementation Notes

- `dispatch_tool()` is currently sync (returns `ContentBlock` directly). For `join_all`, wrap each call in `tokio::task::spawn_blocking(move || dispatch_tool(...))` to parallelize blocking I/O. The closure needs owned copies of `name`, `input`, and `id`. Full ownership pattern:

  ```rust
  let name = name.clone();      // clone for the closure (name is &str from destructuring)
  let name_log = name.clone();  // clone for post-dispatch logging
  let input = input.clone();    // clone for the closure (input is &Value from destructuring)
  let id = id.clone();
  let id_fallback = id.clone(); // clone for JoinError fallback
  let handle = tokio::task::spawn_blocking(move || {
      dispatch_tool(&name, input, &id, &mut |_: &str| {})
  });
  // After join_all, use name_log for logging and id_fallback for JoinError recovery
  ```

  Pure tool inputs are small (paths, patterns), so clone cost is negligible.

- The streaming callback (`&mut |chunk| { eprint!("{chunk}"); }`) is `&mut dyn FnMut(&str)` which is neither `Send` nor `'static` — it cannot be moved into a `spawn_blocking` closure. For the pure path, create a local no-op callback inside each closure: `&mut |_: &str| {}`. Pure tools (Read, Glob, Grep) don't stream output, so the no-op loses nothing.
- `join_all` returns `Vec<Result<ContentBlock, JoinError>>`. `JoinError` indicates a thread panic. Use `id_fallback` (cloned before spawning) to build `ContentBlock::ToolResult { tool_use_id: id_fallback, content: "tool panicked", is_error: Some(true) }`.
- Order preservation: `join_all` returns results in the same order as the input futures. No reordering needed.
- **Tool name preservation for logging:** `ContentBlock::ToolResult` does not contain the tool name, only `tool_use_id`. The parallel path must collect `(String, ContentBlock)` tuples (name + result) rather than bare `ContentBlock` values, so post-dispatch logging can print `tool: Name` with result size/error preview. Clone the name before spawning and pair it with the result after `join_all` completes.
- Pre-dispatch logging: print `tool: {name}` (or `tool: {name}({input})` in verbose mode) for each tool BEFORE spawning the `spawn_blocking` tasks, using the same `color()` format as the sequential path. This happens on the main thread, not inside the closures.
- Post-dispatch logging: after `join_all` completes, iterate the `(name, result)` tuples and print the same result format as the sequential path (size or error preview, conditional on `cli.verbose` and `is_error`).
- The batch classification is O(n) where n is tool count per response (typically 1-5). Negligible overhead.
- The `tool_results.is_empty()` break guard (currently after the sequential for-loop) must be preserved after both the parallel and sequential paths. If somehow the parallel path produces zero results, the inner loop should break.
- This change makes `dispatch_tool` callable from multiple threads simultaneously. Each invocation is independent and only performs read-only filesystem operations for pure tools. No shared mutable state.
- All references use structural landmarks (function names, `for block in`, `tool_iterations`) rather than line numbers, since earlier specs in the implementation order modify the same files.
