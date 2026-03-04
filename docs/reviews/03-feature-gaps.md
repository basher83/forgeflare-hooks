# Feature Gap & Enhancement Analysis: ForgeFlare

**Analyst**: Automated Feature Gaps Agent
**Date**: 2026-02-26
**Scope**: Full codebase, specs, and adjacent configuration

---

## Executive Summary

ForgeFlare is a well-structured coding agent wrapping the Claude API with streaming SSE, 5 tools (Read, Glob, Bash, Edit, Grep), a shell-based hook system, session transcript capture, and convergence tracking. All 9 specs in `specs/` are fully implemented. The codebase is clean, well-tested (119 tests), and under 5,000 total lines across 5 source files. This analysis identifies 42 opportunities across 10 categories.

---

## 1. Planned but Unimplemented Features

All 9 specs are fully implemented. However, several features are referenced in adjacent files but have no specs or implementation:

### 1.1 Subagent Dispatch

- **Priority**: P1 | **Effort**: Large
- `coding-agent.md` explicitly defers subagent support. If ForgeFlare is to operate independently of Claude Code, it needs native subagent dispatch.
- **Approach**: Add a `Subagent` tool that spawns a child `run_turn` with a separate conversation and potentially a different model/max_tokens.

### 1.2 SessionStart / SessionEnd Hooks

- **Priority**: P2 | **Effort**: Small
- ForgeFlare's hook system only supports `PreToolUse`, `PostToolUse`, and `Stop`. SessionStart/End hooks would enable Entire.io integration and session lifecycle management.

### 1.3 UserPromptSubmit Hook

- **Priority**: P3 | **Effort**: Small
- No hook interception point for user input. Would enable prompt filtering, context injection, and audit logging.

---

## 2. Tool Gaps

### 2.1 Write Tool (Dedicated File Creation)

- **Priority**: P1 | **Effort**: Small
- The Edit tool handles file creation via empty `old_str`, but there is no dedicated Write/overwrite tool. ~30 lines in `tools/mod.rs`.

### 2.2 NotebookEdit Tool

- **Priority**: P3 | **Effort**: Medium
- No support for editing Jupyter notebooks. ~100 lines.

### 2.3 WebFetch Tool

- **Priority**: P2 | **Effort**: Medium
- No ability to fetch web content. Would use existing `reqwest` dependency. ~80 lines.

### 2.4 WebSearch Tool

- **Priority**: P3 | **Effort**: Medium
- No web search capability. Requires external search API integration.

### 2.5 LSP/Language Server Integration

- **Priority**: P2 | **Effort**: Large
- No language-server integration for structured code navigation. ~300 lines, complex.

### 2.6 TodoWrite / Task Tracking Tool

- **Priority**: P2 | **Effort**: Small
- No structured task tracking during sessions. ~40 lines.

---

## 3. Hook System Enhancements

### 3.1 Conditional Hooks (Multi-Tool Matching)

- **Priority**: P2 | **Effort**: Small
- `match_tool` only supports exact string matching. Support comma-separated tool names. ~10 lines.

### 3.2 Hook Environment Variables

- **Priority**: P2 | **Effort**: Small
- Hooks receive no ForgeFlare-specific context beyond JSON stdin. Set env vars like `FORGEFLARE_SESSION_ID`, `FORGEFLARE_TOOL_ITERATIONS`. ~15 lines.

### 3.3 Hook Result Modification (PostToolUse)

- **Priority**: P3 | **Effort**: Medium
- PostToolUse hooks cannot modify results. Add optional `modified_result` field for output sanitization.

### 3.4 Hook Performance Metrics

- **Priority**: P2 | **Effort**: Small
- Hook execution time is not tracked. Write timing to convergence.json or a metrics file. ~30 lines.

### 3.5 Pre-API-Call Hook

- **Priority**: P1 | **Effort**: Medium
- No hook fires before the API call. Would enable dynamic system prompt injection, conversation manipulation, and model switching.

---

## 4. Session Management

### 4.1 Cost Tracking

- **Priority**: P1 | **Effort**: Small
- Token usage is captured but no cost calculation exists. Add `--price-input`/`--price-output` CLI flags. ~40 lines.

### 4.2 Session Metrics File

- **Priority**: P2 | **Effort**: Small
- No quantitative metrics file (tool counts, error rates, duration). Write `metrics.json` at session end. ~50 lines.

### 4.3 Session Resume / Checkpoint Restore

- **Priority**: P3 | **Effort**: Large
- No mechanism to resume a session from the JSONL transcript after a crash. ~150 lines.

### 4.4 Real-Time Token/Cost Display

- **Priority**: P2 | **Effort**: Small
- Operator sees no token information during sessions. Print usage summary to stderr after each API call. ~15 lines.

---

## 5. API Enhancements

### 5.1 Prompt Caching (Cache Control Headers)

- **Priority**: P0 | **Effort**: Medium
- ForgeFlare parses cache metrics from responses but never sends cache control headers. The system prompt and tool definitions are re-processed at full cost every API call.
- **Approach**: Add `"cache_control": {"type": "ephemeral"}` to system prompt and tools array. ~30 lines.

### 5.2 Model Switching / Dynamic Model Selection

- **Priority**: P2 | **Effort**: Small
- The model is set once at startup. Enable hooks to signal model switches. ~20 lines.

### 5.3 Extended Thinking (Budget Tokens)

- **Priority**: P2 | **Effort**: Medium
- No extended thinking support. Add `--thinking-budget` CLI flag. ~50 lines.

### 5.4 Rate Limit Awareness

- **Priority**: P2 | **Effort**: Small
- No proactive rate limit management. Track request/token counts and throttle proactively. ~40 lines.

### 5.5 Dynamic System Prompt Injection

- **Priority**: P1 | **Effort**: Small
- System prompt is built once at startup. No mechanism to inject git status, CLAUDE.md content, or other dynamic context. ~30 lines.

---

## 6. Observability

### 6.1 Structured Logging

- **Priority**: P1 | **Effort**: Medium
- All logging uses `eprintln!` with ad-hoc format strings. Add `tracing` crate for structured, filterable logging. ~100 lines.

### 6.2 Performance Tracing

- **Priority**: P2 | **Effort**: Small
- No timing information for API calls, tool dispatch, or hooks. ~30 lines.

### 6.3 Convergence Visualization

- **Priority**: P3 | **Effort**: Medium
- No tool to visualize convergence patterns across multiple sessions. ~100 lines.

---

## 7. Configuration

### 7.1 CLAUDE.md / Project Instructions Loading

- **Priority**: P0 | **Effort**: Small
- ForgeFlare does not read `CLAUDE.md` for project-specific instructions. This file exists in the repo with build/validation instructions but is entirely ignored.
- **Approach**: Read `CLAUDE.md` and `.claude/CLAUDE.md` at startup, append to system prompt. ~20 lines.

### 7.2 Configuration File (forgeflare.toml)

- **Priority**: P2 | **Effort**: Medium
- All configuration is CLI/env. No project-level config file. ~50 lines.

### 7.3 Tool Enable/Disable

- **Priority**: P3 | **Effort**: Small
- All 5 tools always enabled. Add `--disable-tools` flag. ~20 lines.

### 7.4 Configurable Bash Timeout

- **Priority**: P2 | **Effort**: Small
- Bash timeout hardcoded to 120 seconds. Add `timeout` parameter. ~10 lines.

---

## 8. Developer Experience

### 8.1 Interactive Mode (Readline/Rustyline)

- **Priority**: P2 | **Effort**: Medium
- Raw `stdin.read_line()` with no line editing, history, or multi-line support. ~40 lines.

### 8.2 Color/Formatting

- **Priority**: P3 | **Effort**: Small
- `use_color()` function exists but is never called. No ANSI coloring. ~30 lines.

### 8.3 Progress Indicators

- **Priority**: P2 | **Effort**: Small
- No indication of activity during long API calls. ~20 lines.

### 8.4 Multi-Line Input Support

- **Priority**: P2 | **Effort**: Small
- Interactive mode reads one line at a time. ~15 lines.

---

## 9. Testing Infrastructure

### 9.1 Full Loop Integration Test

- **Priority**: P1 | **Effort**: Medium
- No integration tests for the full `run_turn` flow with a mock API server. ~200 lines.

### 9.2 Hook Integration Tests

- **Priority**: P2 | **Effort**: Small
- No tests for hook-loop interaction. ~100 lines.

### 9.3 Property-Based Testing / Fuzzing

- **Priority**: P3 | **Effort**: Medium
- No property-based testing for SSE parser or conversation trim logic. ~100 lines.

### 9.4 Snapshot Tests

- **Priority**: P2 | **Effort**: Small
- No format stability tests for JSONL, convergence.json, or context.md output. ~50 lines.

---

## 10. Resilience & Recovery

### 10.1 Graceful Shutdown (Signal Handling)

- **Priority**: P1 | **Effort**: Small
- No signal handler. Ctrl+C kills immediately. Session may have incomplete transcript and no final convergence state. ~40 lines.

### 10.2 Conversation Persistence / Autosave

- **Priority**: P2 | **Effort**: Medium
- Conversation only in memory. Write to `.forgeflare/conversation.json` after each turn. ~60 lines.

### 10.3 Token Budget / Hard Cost Limit

- **Priority**: P1 | **Effort**: Small
- No maximum token budget. Add `--max-tokens-budget` CLI flag. ~15 lines.

### 10.4 Tool Output Size Limits

- **Priority**: P2 | **Effort**: Small
- Tool results added to conversation without size limits. Add global truncation. ~20 lines.

### 10.5 Concurrent Instance Detection

- **Priority**: P3 | **Effort**: Small
- Nothing prevents two instances running in the same directory. Add PID file. ~25 lines.

---

## Priority Summary

### P0 (Critical)
| Feature | Effort | Category |
|---------|--------|----------|
| Prompt Caching (5.1) | Medium | API |
| CLAUDE.md Loading (7.1) | Small | Config |

### P1 (High)
| Feature | Effort | Category |
|---------|--------|----------|
| Subagent Dispatch (1.1) | Large | Features |
| Write Tool (2.1) | Small | Tools |
| Pre-API-Call Hook (3.5) | Medium | Hooks |
| Cost Tracking (4.1) | Small | Session |
| Dynamic System Prompt (5.5) | Small | API |
| Structured Logging (6.1) | Medium | Observability |
| Full Loop Integration Test (9.1) | Medium | Testing |
| Graceful Shutdown (10.1) | Small | Resilience |
| Token Budget (10.3) | Small | Resilience |

### P2 (Medium)
| Feature | Effort | Category |
|---------|--------|----------|
| SessionStart/End Hooks (1.2) | Small | Features |
| WebFetch Tool (2.3) | Medium | Tools |
| LSP Integration (2.5) | Large | Tools |
| TodoWrite Tool (2.6) | Small | Tools |
| Conditional Hooks (3.1) | Small | Hooks |
| Hook Env Vars (3.2) | Small | Hooks |
| Hook Metrics (3.4) | Small | Hooks |
| Session Metrics (4.2) | Small | Session |
| Token Display (4.4) | Small | Session |
| Model Switching (5.2) | Small | API |
| Extended Thinking (5.3) | Medium | API |
| Rate Limiting (5.4) | Small | API |
| Performance Tracing (6.2) | Small | Observability |
| Config File (7.2) | Medium | Config |
| Bash Timeout (7.4) | Small | Config |
| Readline (8.1) | Medium | DX |
| Progress Indicators (8.3) | Small | DX |
| Multi-Line Input (8.4) | Small | DX |
| Hook Integration Tests (9.2) | Small | Testing |
| Snapshot Tests (9.4) | Small | Testing |
| Conversation Persistence (10.2) | Medium | Resilience |
| Tool Output Limits (10.4) | Small | Resilience |

### P3 (Nice-to-Have)
| Feature | Effort | Category |
|---------|--------|----------|
| UserPromptSubmit Hook (1.3) | Small | Features |
| NotebookEdit Tool (2.2) | Medium | Tools |
| WebSearch Tool (2.4) | Medium | Tools |
| Hook Result Modification (3.3) | Medium | Hooks |
| Session Resume (4.3) | Large | Session |
| Convergence Visualization (6.3) | Medium | Observability |
| Tool Enable/Disable (7.3) | Small | Config |
| Color/Formatting (8.2) | Small | DX |
| Property Testing (9.3) | Medium | Testing |
| Concurrent Detection (10.5) | Small | Resilience |

---

## Recommended Implementation Order

1. **CLAUDE.md Loading** -- Small effort, critical for autonomous operation
2. **Prompt Caching** -- Immediate cost/latency reduction on every session
3. **Write Tool** -- Fills the most obvious tool gap
4. **Graceful Shutdown** -- Prevents data loss on interrupt
5. **Token Budget** -- Essential safety net for autonomous loops
6. **Cost Tracking** -- Enables informed decisions
7. **Structured Logging** -- Unlocks downstream observability
8. **Full Loop Integration Test** -- Confidence for future changes
9. **Dynamic System Prompt** -- Context-aware agents
10. **Pre-API-Call Hook** -- Runtime system prompt and model switching
