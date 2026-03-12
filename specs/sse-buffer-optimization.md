---
status: Complete
created: 2026-03-11
---

# Optimize SSE Buffer Reallocation

**Target:** Eliminate unnecessary String allocation in the SSE parser's event extraction loop by using `drain` or offset tracking instead of slice-to-String copies

---

## Why

The SSE parser in `src/api.rs` processes streaming chunks by accumulating bytes into a `buffer: String` and splitting on `\n\n` (double newline, the SSE event boundary). The current extraction:

```rust
let event_block = buffer[..pos].to_string();
buffer = buffer[pos + 2..].to_string();
```

Both lines allocate a new String. The second line is the problem: every time an event is extracted, the remaining buffer is copied into a fresh allocation. For a response with N SSE events where the average remaining buffer is M bytes, total copy work is O(N * M). In practice this is not catastrophic because SSE events are small (typically 100-500 bytes each) and arrive incrementally, so M stays small. But it's gratuitous allocation that's trivial to fix.

---

## Requirements

**R1. Eliminate Remaining-Buffer Reallocation**
- After extracting an event from the buffer, advance past it without copying the remaining content into a new String
- Two acceptable approaches:
  - `buffer.drain(..pos + 2)` — removes consumed bytes in-place, shifts remaining bytes to front. Single `memmove` instead of allocate + copy + free.
  - Offset tracking — maintain a `start: usize` index and only compact when the buffer exceeds a threshold. More complex, not worth it for SSE-sized buffers.
- Prefer `drain` for simplicity.

**R2. Eliminate Event-Block Allocation**
- `event_block = buffer[..pos].to_string()` creates a copy just to iterate lines
- Instead, take a slice reference `&buffer[..pos]` and iterate lines directly
- This avoids one allocation per event

**R3. Preserve Exact Parsing Behavior**
- The parser must produce identical output for all existing SSE test cases
- Event boundary detection (double newline) unchanged
- `data:` line extraction unchanged
- JSON parsing of event data unchanged
- All content_block_start, content_block_delta, content_block_stop, message_start, message_delta, error event handling unchanged

---

## Architecture

```text
Current:
  buffer.push_str(&chunk);
  while let Some(pos) = buffer.find("\n\n") {
      let event_block = buffer[..pos].to_string();     // ALLOC 1
      buffer = buffer[pos + 2..].to_string();           // ALLOC 2
      for line in event_block.lines() { ... }
  }

After:
  buffer.push_str(&chunk);
  while let Some(pos) = buffer.find("\n\n") {
      {
          let event_block = &buffer[..pos];              // NO ALLOC (slice)
          for line in event_block.lines() { ... }
      }
      buffer.drain(..pos + 2);                           // IN-PLACE (memmove)
  }
```

Changes to existing code:

1. `src/api.rs`, `parse_sse_stream()` — Replace the two `.to_string()` calls in the event extraction loop (lines 213-214) with a slice reference and `drain`. The borrow scope of the slice must end before `drain` is called (use a block `{ ... }` to scope the borrow).

---

## Success Criteria

- [ ] No `.to_string()` calls on buffer slices in the SSE event extraction loop
- [ ] All 6 existing SSE parser tests pass unchanged
- [ ] `parse_sse_text_response` — text streaming and block assembly
- [ ] `parse_sse_tool_use_response` — tool input JSON accumulation
- [ ] `parse_sse_error_event_transient` — overloaded_error handling
- [ ] `parse_sse_error_event_permanent` — invalid_request_error handling
- [ ] `parse_sse_missing_stop_reason` — connection drop detection
- [ ] `parse_sse_usage_from_message_start_and_delta` — cache token parsing
- [ ] `cargo clippy -- -D warnings` clean

---

## Non-Goals

- Rewriting the SSE parser to use a streaming parser library (e.g., `eventsource-stream`)
- Pre-allocating the buffer with a capacity hint (the buffer is already created with `String::new()` and grows as needed — fine for SSE)
- Benchmarking the change (the improvement is obvious from first principles — fewer allocations is fewer allocations)
- Changing the chunk accumulation strategy (`buffer.push_str(&String::from_utf8_lossy(&chunk))` is fine)
- Handling partial UTF-8 sequences across chunk boundaries (existing behavior, out of scope)

---

## Implementation Notes

- The borrow checker requires that the slice reference `&buffer[..pos]` is dropped before `buffer.drain()` is called. Use a block scope:

```rust
while let Some(pos) = buffer.find("\n\n") {
    {
        let event_block = &buffer[..pos];
        for line in event_block.lines() {
            // ... existing line processing ...
        }
    }
    buffer.drain(..pos + 2);
}
```

- `String::drain(range)` returns a `Drain` iterator. We don't need the drained content, so just let it drop: `buffer.drain(..pos + 2);` (the semicolon drops the iterator).
- The `drain` approach does an internal `memmove` to shift remaining bytes to the buffer's start. For SSE-sized buffers (typically a few KB at most), this is effectively free.
- This has no dependency on other specs. It can be implemented in any order.
- This is the lowest-priority of the 5 specs. The performance impact is negligible in practice, but the fix is trivial and makes the code cleaner.
