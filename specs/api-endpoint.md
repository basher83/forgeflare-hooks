# API Endpoint Configuration Specification

**Status:** Active
**Target:** Route API requests through a configurable endpoint, defaulting to the tailnet OAuth proxy

---

## Why

Forgeflare hardcodes `https://api.anthropic.com/v1/messages` as the API endpoint and requires `ANTHROPIC_API_KEY` on every run. An OAuth proxy (`anthropic-oauth-proxy`) is deployed on the tailnet at `https://anthropic-oauth-proxy.tailfb3ea.ts.net`, managing authentication via Claude Max subscription pooling with automatic token refresh and quota failover. The proxy accepts bare requests (no client credentials), injects Bearer tokens, and forwards to Anthropic upstream.

The common case is running forgeflare on the tailnet where the proxy handles authentication. Direct API key auth is the escape hatch for off-tailnet use or testing. The default should reflect the common case: zero-config launch on the tailnet.

---

## Requirements

**R1. Configurable API Base URL**
- Add `api_url` field to `AnthropicClient` storing the base URL (without path)
- Default value: `https://anthropic-oauth-proxy.tailfb3ea.ts.net`
- Override via CLI `--api-url <URL>` or environment variable `ANTHROPIC_API_URL`
- Precedence: CLI arg > env var > default
- The `/v1/messages` path is appended at request time, not stored in `api_url`

**R2. Optional API Key**
- `ANTHROPIC_API_KEY` becomes optional (not required at startup)
- When set: attach `x-api-key` header to every request (passthrough mode — proxy forwards it unchanged)
- When unset: send no authentication header (OAuth pool mode — proxy injects Bearer token)
- Remove the `MissingApiKey` error variant from `AgentError`

**R3. CLI Integration**
- Add `--api-url` argument to the `Cli` struct via clap
- Default value comes from `ANTHROPIC_API_URL` env var, falling back to `https://anthropic-oauth-proxy.tailfb3ea.ts.net`
- Pass the resolved URL into `AnthropicClient::new()`

**R4. AnthropicClient Construction**
- `AnthropicClient::new()` signature changes to accept `api_url: &str`
- Store `api_url: String` and `api_key: Option<String>` on the struct
- No validation of the URL at construction time (reqwest will surface connection errors at request time)

**R5. Request Construction**
- `send_message()` constructs the endpoint as `format!("{}/v1/messages", self.api_url)`
- Conditionally attach `x-api-key` header only when `self.api_key.is_some()`
- Always send `anthropic-version: 2023-06-01` header regardless of endpoint (proxy passes it through, direct API requires it)

---

## Architecture

```text
AnthropicClient
  ├── api_url: String          # base URL (no trailing slash, no path)
  ├── api_key: Option<String>  # None when using OAuth proxy
  └── client: reqwest::Client  # unchanged

Cli
  └── --api-url: String        # new arg, env = ANTHROPIC_API_URL

main()
  └── AnthropicClient::new(cli.api_url)  # pass resolved URL

send_message()
  ├── POST {api_url}/v1/messages
  ├── if api_key.is_some() → header("x-api-key", key)
  └── header("anthropic-version", "2023-06-01")  # always
```

Changes to existing code:

1. `api.rs` — `AnthropicClient` struct gains `api_url: String`, `api_key` becomes `Option<String>`, `new()` accepts `api_url: &str`, `send_message()` uses `self.api_url` and conditionally attaches auth header, remove `MissingApiKey` variant
2. `main.rs` — `Cli` struct gains `--api-url` with env/default, pass to `AnthropicClient::new()`

---

## Proxy Compatibility

The tailnet OAuth proxy at `anthropic-oauth-proxy.tailfb3ea.ts.net` operates in two modes:

**Passthrough mode** (client provides API key):
- Proxy forwards `x-api-key` header unchanged
- Proxy injects `anthropic-beta` header
- No body modification

**OAuth pool mode** (no client credentials):
- Proxy strips any client `Authorization` header
- Proxy injects `Authorization: Bearer {access_token}` from managed pool
- Proxy injects required `anthropic-beta` flags
- Proxy prepends required system prompt prefix for non-Haiku models
- Proxy handles 429 quota exhaustion with transparent failover

Forgeflare does not need to know which mode the proxy runs in. The conditional `x-api-key` header is the only client-side difference: present for passthrough, absent for OAuth pool.

---

## Success Criteria

- [ ] `cargo run` on tailnet works with zero environment variables
- [ ] `ANTHROPIC_API_KEY=sk-... cargo run -- --api-url https://api.anthropic.com` works for direct API access
- [ ] `ANTHROPIC_API_URL=https://api.anthropic.com ANTHROPIC_API_KEY=sk-... cargo run` works via env vars
- [ ] SSE streaming works identically through the proxy (proxy streams response bodies)
- [ ] Existing tests pass (no behavioral change when api_key is provided)
- [ ] `--api-url` appears in `--help` output
- [ ] `--verbose` prints the resolved API URL at startup

---

## Non-Goals

- Provider abstraction layer or auth strategy enum (the proxy handles auth transformation)
- URL validation or reachability checks at startup (fail at request time with clear errors)
- Retry logic for proxy connection failures (spec explicitly rejects automatic retry)
- Proxy health checks or discovery (static URL, tailnet DNS handles resolution)
- Configuration file support (env vars and CLI args are sufficient)
- TLS certificate pinning or custom CA bundles (reqwest uses system roots, Tailscale handles certs)

---

## Implementation Notes

- The `api_url` default should not have a trailing slash. `format!("{}/v1/messages", self.api_url)` produces the correct path either way, but consistency matters for verbose logging.
- clap supports `env = "ANTHROPIC_API_URL"` on `#[arg()]` to read from environment with CLI override. This gives the three-tier precedence (CLI > env > default) for free.
- The `anthropic-version` header must always be sent. In passthrough mode, the proxy forwards it. In OAuth pool mode, the proxy replaces it, but sending it is harmless.
- Removing `MissingApiKey` from `AgentError` is a breaking change to the error enum but there are no downstream consumers — forgeflare is a standalone binary.
- The AGENTS.md "Build & Run" section should be updated to reflect that `ANTHROPIC_API_KEY` is no longer required on the tailnet. The new invocation is just `cargo run`.
