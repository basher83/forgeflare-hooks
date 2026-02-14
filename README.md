# ForgeFlare

A streaming coding agent powered by Claude, built as a single Rust binary. ForgeFlare wraps the Anthropic Messages API with tool dispatch, a programmable hook system for controlling agent behavior, and convergence tracking for autonomous operation.

## Quick Start

```bash
git clone git@github.com:basher83/forgeflare-hooks.git
cd forgeflare-hooks
cargo build
```

On the tailnet (default, no API key needed):

```bash
cargo run
```

Off tailnet with a direct API key:

```bash
ANTHROPIC_API_KEY=sk-... cargo run -- --api-url https://api.anthropic.com
```

## Architecture

ForgeFlare runs an agentic loop: read user input, call the Claude API with streaming SSE, dispatch tool calls, and repeat until the model stops or a convergence signal fires. Five tools are available to the agent (Read, Glob, Bash, Edit, Grep), with pure tools (Read, Glob, Grep) executing concurrently and mutating tools (Bash, Edit) running sequentially.

The hook system is the distinguishing feature. External shell scripts can gate tool execution (guard hooks block dangerous commands), observe agent activity, signal convergence, and run cleanup on stop. Hooks communicate via JSON on stdin/stdout, with guard hooks fail-closed and observe/post/stop hooks fail-open.

```text
src/
  main.rs       Agentic loop, context trimming, retry logic
  api.rs        Anthropic Messages API client with SSE streaming
  tools/mod.rs  Tool schemas (via macro) and dispatch router
  hooks.rs      Hook runner: guard/observe/post/stop lifecycle
  session.rs    Session transcript writer (JSONL + metadata)
```

## Hook System

Hooks are configured in `.forgeflare/hooks.toml` and run as shell executables that receive JSON on stdin and return JSON on stdout. Three lifecycle events are supported:

**PreToolUse** runs before each tool call in two phases. Guard hooks can block tool execution (fail-closed: timeouts, crashes, and invalid JSON all result in blocking). Observe hooks run after guards with the guard outcome as context (fail-open).

**PostToolUse** runs after each tool completes. Hooks can return a `signal` action to indicate convergence. Observations accumulate in `.forgeflare/convergence.json`.

**Stop** fires when the agent turn ends, receiving the stop reason and token totals. The convergence file gets a `final` entry with the termination state.

```toml
# .forgeflare/hooks.toml
[[hooks]]
event = "PreToolUse"
phase = "guard"
command = "/path/to/guard.sh"
match_tool = "Bash"
timeout_ms = 5000

[[hooks]]
event = "PostToolUse"
command = "/path/to/convergence-checker.sh"

[[hooks]]
event = "Stop"
command = "/path/to/cleanup.sh"
```

## Convergence Tracking

PostToolUse hooks can signal convergence by returning `{"action": "signal", "signal": "converged", "reason": "..."}`. These observations accumulate in `.forgeflare/convergence.json` with atomic writes (temp file + rename). When the agent turn ends, a `final` entry records the stop reason, tool iterations, total tokens, and timestamp.

This makes ForgeFlare suitable as the inner engine for autonomous loops where a bash supervisor needs to detect when the agent has converged and should stop.

## Specs

Feature specs live in `specs/` and document the design rationale and acceptance criteria for each capability. See `specs/README.md` for the full index and implementation order.
