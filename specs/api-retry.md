# API Retry for Transient Failures

**Status:** Active
**Target:** Retry transient API errors (429, 503, 529, network timeouts) with exponential backoff instead of breaking the inner loop

---

## Why

Forgeflare currently treats every API error identically: `recover_conversation()` then break the inner loop (the `Err(e) =>` arm in the `match client.send_message(...).await` block). This is fine for interactive use where the human retries, but fatal for autonomous Ralph loops where nobody is watching. A 429 rate limit or 503 overload is a temporary condition — the request will succeed in seconds. Breaking the loop forces the bash harness to restart the entire iteration with fresh context, wasting the current iteration's accumulated work.

The `retry-after` header is already parsed (in the non-200 response handler in `send_message()`) but only used in the error message string. The data is there; the retry logic isn't.

**Supersedes** `coding-agent.md` line 128: "No automatic retry; failures return to user for decision." That was correct for the interactive-only agent. Autonomous operation requires retry.

---

## Requirements

**R1. Classify API Errors as Transient or Permanent**

Classification operates on the `AgentError` enum variants. Every variant must have a classification:

- `HttpError { status, .. }` — classify by status code:
  - Transient: 429 (rate limited), 503 (overloaded), 529 (overloaded), unknown >= 500
  - Permanent: 400 (bad request), 401 (auth failure), 403 (forbidden), unknown 4xx (not 429)
- `StreamTransient(String)` — always transient. Covers SSE-level `error` events indicating overload (e.g., `"stream error: Overloaded"`, type `overloaded_error` or `api_error`) and stream ended without stop_reason (connection drop mid-stream)
- `StreamParse(String)` — always permanent. Malformed JSON, protocol violations, SSE `error` events with type `invalid_request_error`
- `Api(reqwest::Error)` — classify by inspecting the reqwest error:
  - Transient: `e.is_timeout()` or `e.is_connect()` (network timeout, connection reset, connection refused during connect)
  - Permanent: all other `Api` errors (DNS resolution failure for bad URL, TLS certificate error, etc.)
- `Json(serde_json::Error)` — always permanent (malformed JSON in SSE data, same category as `StreamParse`)

SSE `error` event classification: in the SSE data payload JSON, inspect `p["error"]["type"]` (NOT the top-level `p["type"]` which is always `"error"` — the classification type is nested under the `error` object). `overloaded_error`, `api_error`, and `rate_limit_error` produce `StreamTransient`. `invalid_request_error` produces `StreamParse`. Unknown or absent types default to `StreamTransient` (server-side errors are usually temporary).

**R2. Retry with Exponential Backoff**
- On transient error: sleep, then retry the same `send_message()` call
- Backoff schedule: 2s, 4s, 8s, 16s (exponential, base 2) — 4 entries for 4 retries
- If the error is `HttpError` and `retry_after` is `Some(n)`, use `n` seconds instead of the backoff schedule (capped at 60s). `retry_after` only applies to `HttpError` variants (HTTP response headers are not available on `StreamTransient` or `Api` errors).
- Maximum 4 retry attempts after the initial call (5 total `send_message()` calls maximum: 1 initial + 4 retries)
- After max retries exhausted: fall through to existing error handling (recover_conversation + break)

**R3. Preserve Existing Error Path for Permanent Failures**
- Permanent errors skip retry entirely — immediate `recover_conversation()` + break
- No behavioral change for 400/401/403 errors
- Error message displayed to user on every attempt (not just the final failure)

**R4. Logging**
- Log each retry attempt: `[retry] Attempt {n}/4: {status} — waiting {delay}s` (n is 1-indexed: 1 through 4)
- Log when retry-after header overrides the backoff: `[retry] Using retry-after: {n}s`
- Use existing `color()` helper for consistent formatting

---

## Architecture

```text
send_message() → Err(e)
  │
  ├─ classify_error(&e) → Transient
  │    ├─ attempts < MAX_RETRIES?
  │    │    ├─ yes → log, sleep(backoff), retry send_message()
  │    │    └─ no  → fall through to permanent path
  │    └─ extract retry_after from error context if available
  │
  └─ classify_error(&e) → Permanent
       └─ recover_conversation() + break (existing behavior)
```

Changes to existing code:

1. `api.rs` — Split error variants for classification:
   - Add `HttpError { status: u16, retry_after: Option<u64>, body: String }` for HTTP-level failures (non-200 responses)
   - Add `StreamTransient(String)` for transient stream errors (overload events, connection drops)
   - Keep `StreamParse(String)` for permanent parse failures (malformed JSON, protocol violations)
   - Keep `Api(reqwest::Error)` — classify at the call site using `is_timeout()`/`is_connect()`
   - Keep `Json(serde_json::Error)` — classify as permanent at the call site
   - In the SSE parser: overload/api_error `error` events and missing stop_reason produce `StreamTransient`, not `StreamParse`. `invalid_request_error` events produce `StreamParse`.
   - Add `classify_error(e: &AgentError) -> ErrorClass` function in `api.rs` that matches on all five variants per R1. `ErrorClass` is an enum with two variants: `Transient` and `Permanent`.
2. `main.rs` — Replace the entire `match client.send_message(...).await { Ok(r) => r, Err(e) => ... }` block with a retry loop. All retry logic lives in `main.rs`. No retry logic inside `send_message()` or the SSE parser — stream-level transient errors propagate out of `send_message()` and are retried at the call site. The retry loop wraps both the call and the match — it is NOT a loop inside the Err arm.

---

## Success Criteria

- [ ] 429 response retried after `retry-after` seconds (or 2s default)
- [ ] 503/529 response retried with exponential backoff
- [ ] Network timeout retried with exponential backoff
- [ ] 400 response triggers immediate break (no retry)
- [ ] Max 4 retries before falling through to existing error handling
- [ ] Retry attempts logged with attempt count and delay
- [ ] Existing tests pass (no behavioral change for non-error paths)
- [ ] `recover_conversation()` still called on final failure

---

## Non-Goals

- Circuit breaker pattern (overkill for a single-client agent)
- Jitter on backoff delays (not enough concurrent clients to cause thundering herd)
- Retry of stream parse errors mid-response (stream corruption is not transient)
- Persistent retry state across outer loop iterations (retry counter resets each API call)
- Configuration of retry parameters via CLI args (hardcoded defaults are fine)

---

## Implementation Notes

- The retry loop wraps the existing `client.send_message()` call, not the entire inner loop. The conversation state doesn't change between retries — the same messages are re-sent.
- `tokio::time::sleep(Duration::from_secs(delay))` for the backoff delay. Already in the tokio dependency.
- The `AgentError` enum currently stuffs HTTP status into a string via `StreamParse(format!("API returned {status}..."))`. The retry logic needs the raw status code to classify. Add an `HttpError { status: u16, retry_after: Option<u64>, body: String }` variant. The display impl formats it the same way for user-facing output. Use `HttpError` consistently — not `HttpStatus`, not any other name.
- The error branch in the `send_message` match block becomes a `for attempt in 0..MAX_RETRIES` loop with a `match classify_error(&e)` inside. Keep it flat — no helper functions unless the retry loop exceeds 30 lines. Log format uses 1-indexed attempt: `for attempt in 1..=MAX_RETRIES` or `attempt + 1` in the format string.
- `retry-after` header value is already extracted as a string in the non-200 response handler in `send_message()`. Parse it as `u64` (seconds). If parsing fails (the header can be an HTTP-date per RFC 7231 or a fractional value like "1.5" — Anthropic uses integer seconds in practice), fall back to the exponential backoff schedule. A value of 0 means retry immediately (no sleep). Attach the parsed value to the `HttpError` variant. Cap at 60s.
- On retry after a stream-level transient error (`StreamTransient`), the SSE parser has already printed partial text to stdout before the error. When the retry succeeds, the full response streams again, producing duplicate text output. This is acceptable — Ralph loops capture tool results, not streamed text. Print `[retry] Retrying from beginning of response...` before retrying a `StreamTransient` error so duplicate output is contextualized.
- All references use structural landmarks (function names, match patterns) rather than line numbers, since earlier specs in the implementation order modify the same files.
