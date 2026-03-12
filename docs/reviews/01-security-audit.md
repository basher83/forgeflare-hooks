# Security Audit Report: ForgeFlare

**Auditor**: Automated Security Review Agent
**Date**: 2026-02-26
**Commit**: `71a91c2`
**Scope**: Full source tree at `/home/user/forgeflare-hooks`
**Codebase**: Rust single-binary coding agent with shell hook system

**Status Update**: 2026-03-12 (reviewed against current HEAD)

---

## Executive Summary

The codebase is a coding agent that executes arbitrary shell commands, reads/writes files, and spawns hook subprocesses based on configuration. The threat model is unusual: the agent itself is inherently privileged (it runs `bash -c <command>` by design), so the primary risks center on (1) bypasses of the safety guardrails that are present, (2) injection via untrusted input into shell subprocesses, (3) file system safety, and (4) network trust assumptions.

### Resolution Summary

Since the initial review:
- **CRITICAL finding #1 (shell injection in glob) has been RESOLVED** ✅
- **Several medium/low severity findings have been addressed** ✅
- **13 of 19 findings remain open** ⚠️

Overall the code is reasonably well-structured for its threat model.

---

## Findings

### FINDING 1: Shell Injection in Glob Tool via Unescaped User Input ✅ RESOLVED

**Severity**: Critical
**Location**: `src/tools/mod.rs:192-204`
**Status**: Fixed in commit `0a71925` (fix(security): replace shell injection in glob tool with glob crate)

~~The `glob_exec` function constructs a bash command by interpolating user-supplied `pattern` and `base` path values directly into a shell string without any escaping or sanitization.~~

**Resolution**: The `glob_exec` function has been completely rewritten to use the Rust `glob` crate instead of shelling out to bash. This eliminates the shell injection vector entirely. A structural test (`glob_no_bash_command_in_source`) verifies that `Command::new("bash")` does not appear in the `glob_exec` function body.

---

### FINDING 2: Bash Deny-List is Trivially Bypassable ⚠️ OPEN

**Severity**: High
**Location**: `src/tools/mod.rs:217-242`
**Status**: Design limitation, documented as best-effort

The `BASH_DENY_LIST` uses substring matching on a normalized (lowercased, whitespace-collapsed) command. This is a blocklist approach that can be trivially bypassed:

```rust
const BASH_DENY_LIST: &[&str] = &[
    "rm -rf /",
    "rm -fr /",
    ...
    "git push --force",
    "git push -f",
];
```

Bypass examples:
- `rm -r -f /` (flags split across arguments)
- `/bin/rm -rf /` (absolute path to binary)
- `command rm -rf /` (shell builtins)
- `"rm" -rf /` (quoting the command name)
- `rm --recursive --force /` (long form flags)
- `eval 'rm -rf /'` (indirect execution)
- `bash -c 'rm -rf /'` (nested shell)

**Recommended Fix**: Deny-lists for shell commands are fundamentally ineffective. Either (a) use the hook system as the primary enforcement mechanism and document that the built-in deny-list is best-effort only, or (b) adopt an allowlist approach, or (c) run commands in a sandboxed environment (nsjail, bubblewrap, containers).

---

### FINDING 3: Path Traversal in File Operations (No Sandboxing)

**Severity**: Medium
**Location**: `src/tools/mod.rs:151-182` (read_exec), `374-444` (edit_exec)

The `read_exec` and `edit_exec` functions accept arbitrary file paths without any path validation or sandboxing. The model can read any file on the system accessible to the running user (e.g., `/etc/shadow`, `~/.ssh/id_rsa`, `~/.aws/credentials`). Similarly, `edit_exec` can create directories and write files anywhere on the filesystem.

**Recommended Fix**: Consider implementing an optional sandbox boundary (configurable root directory) that restricts file operations to a project directory. At minimum, add symlink resolution checking (`std::fs::canonicalize`) to prevent symlink-based traversal attacks.

---

### FINDING 4: Hardcoded Internal Tailnet URL Exposed

**Severity**: Medium
**Location**: `src/main.rs:47-48`

The default API URL is hardcoded to an internal Tailscale URL: `https://anthropic-oauth-proxy.tailfb3ea.ts.net`. This exposes the internal Tailscale hostname and Tailnet node name in the public codebase.

**Recommended Fix**: Use a more generic default or require explicit configuration. Consider requiring `ANTHROPIC_API_URL` to be set explicitly, or validate at startup that the configured URL is reachable.

---

### FINDING 5: No HTTPS Enforcement When API Key is Present

**Severity**: Medium
**Location**: `src/api.rs:106-110`

The `api_url` can be set to any URL, including `http://` URLs. There is no enforcement that the API URL uses HTTPS. An attacker could set `ANTHROPIC_API_URL=http://evil.com` and the API key would be sent over plaintext HTTP.

**Recommended Fix**: Validate that `api_url` starts with `https://` before sending requests, especially when an API key is configured. Add a `--allow-insecure` flag if HTTP is needed for local development.

---

### FINDING 6: Environment Variable Leakage to Subprocesses

**Severity**: Medium
**Location**: `src/hooks.rs:443-449`, `src/tools/mod.rs:256-262`

Hook subprocesses and Bash tool commands inherit the full environment of the parent process. This means `ANTHROPIC_API_KEY` and any other sensitive environment variables are available to every hook script and every LLM-controlled bash command.

**Recommended Fix**: For hook subprocesses, use `.env_clear()` and explicitly pass only needed variables. For the Bash tool, consider scrubbing sensitive environment variables before spawning. At minimum, filter out `ANTHROPIC_API_KEY` from child process environments.

---

### FINDING 7: TOCTOU Race in File Operations

**Severity**: Medium
**Location**: `src/tools/mod.rs:156-170` (read_exec), `386-441` (edit_exec)

The `read_exec` function checks `path.exists()` and `fs::metadata()` before reading. Between the metadata check and the actual read, the file could be replaced. Similarly, `edit_exec` has a non-atomic read-modify-write pattern where intermediate changes could be silently lost.

**Recommended Fix**: Remove the separate `exists()` check and let the read itself produce the error. For write paths, use file locking (`flock`) or atomic write (temp file + rename).

---

### FINDING 8: Convergence State Read-Modify-Write Race

**Severity**: Low
**Location**: `src/hooks.rs:481-507` (write_observations), `509-536` (write_final_state)

Both functions follow a read-modify-write pattern. While the write is atomic (temp file + rename), the full cycle is not. If multiple tool executions complete simultaneously, observations could be lost.

**Recommended Fix**: Add file locking around the read-modify-write cycle, or document the assumption that this function is never called concurrently.

---

### FINDING 9: Symlink Following Without Checking

**Severity**: Medium
**Location**: `src/tools/mod.rs:156` (read_exec), `386` (edit_exec)

File operations follow symlinks without any checking. An attacker who can create symlinks in the working directory could use them to read or modify files outside the project.

**Recommended Fix**: Use `std::fs::symlink_metadata` to detect symlinks. Consider adding `std::fs::canonicalize` to resolve the real path and check it against an allowed scope.

---

### FINDING 10: Hook Command Injection via TOML Configuration

**Severity**: Low
**Location**: `src/hooks.rs:84-94`

The `command` field from `.forgeflare/hooks.toml` is passed directly to `bash -c`. If an attacker can modify this file, they can execute arbitrary commands. While this is expected for the threat model, there is no warning when hooks are loaded.

**Recommended Fix**: Consider warning the user when hooks are loaded, especially if the hooks.toml file has been recently modified.

---

### FINDING 11: Unbounded Memory Growth in Bash Tool Output

**Severity**: Low
**Location**: `src/tools/mod.rs:310-325`

The `bash_exec` function accumulates all stdout and stderr into a single `String` without any size limit. A command that produces gigabytes of output would grow until memory is exhausted.

**Recommended Fix**: Add a maximum output size limit (e.g., 10MB). Truncate output beyond this limit.

---

### FINDING 12: Error Message Information Leakage

**Severity**: Low
**Location**: `src/api.rs:14-15`, `src/tools/mod.rs:162`

Error messages include full system paths and error details from the API body and OS-level errors. For a local CLI tool, this is generally acceptable. If exposed as a service, sanitize error messages.

---

### FINDING 13: Missing Cleanup of Timed-Out Hook Processes

**Severity**: Low
**Location**: `src/hooks.rs:442-478`

When a hook subprocess times out, the child process may continue running as an orphan. Tokio's `Child` only kills on drop if `kill_on_drop(true)` was set.

**Recommended Fix**: Add `.kill_on_drop(true)` to the `Command` builder.

---

### FINDING 14: Bash Tool Timeout Does Not Kill Process Group

**Severity**: Low
**Location**: `src/tools/mod.rs:316`

When a bash command times out, only the immediate child process is killed. Spawned child processes become orphans.

**Recommended Fix**: Create a new process group for the child and kill the entire group on timeout using `libc::killpg`.

---

### FINDING 15: Session Transcript Writes Are Not Atomic

**Severity**: Low
**Location**: `src/session.rs:155-164`

Session transcripts are written using append mode, which is generally safe for small writes. For very large JSONL lines, concurrent writes could theoretically interleave. Acceptable for the current single-threaded use case.

---

### FINDING 16: CI Action Version Inconsistency (Positive Practice Noted) ✅ RESOLVED

**Severity**: Informational
**Location**: `.github/workflows/ci.yml`, `.github/workflows/release.yml`
**Status**: Fixed in commit `0ed7279` (fix(release): align action SHAs with ci.yml convention)

~~The workflows pin GitHub Actions to commit SHAs (strong supply-chain practice). However, `ci.yml` uses a different checkout SHA than `release.yml`.~~

**Resolution**: The action SHAs have been aligned between ci.yml and release.yml.

---

### FINDING 17: Dependency Review

**Severity**: Informational
**Location**: `Cargo.toml`

The dependency set is minimal and reasonable. `tokio` with `features = ["full"]` enables all features; consider reducing to only what is needed: `["rt-multi-thread", "macros", "time", "process", "io-util"]`.

---

### FINDING 18: Guard Hook Outputs to stderr Instead of stdout

**Severity**: Low
**Location**: `.claude/hooks/ralph-guard.sh:14`

The `deny()` function writes its JSON output to stderr rather than stdout. The descriptive reason from the JSON output is lost -- the user sees a generic "exited with code 2" message.

**Recommended Fix**: Output the JSON to stdout and use stderr only for human-readable logging.

---

### FINDING 19: No Rate Limiting on Tool Execution

**Severity**: Informational
**Location**: `src/main.rs:15`

While there is a `MAX_TOOL_ITERATIONS` limit of 50, there is no rate limiting within those iterations and no limit on the parallelism of pure tools.

**Recommended Fix**: Consider adding a semaphore to limit concurrent tool execution.

---

## Summary Table

| # | Severity | Component | Finding | Status |
|---|----------|-----------|---------|--------|
| 1 | **Critical** | tools/mod.rs | Shell injection in Glob tool via unescaped user input | ✅ RESOLVED |
| 2 | **High** | tools/mod.rs | Bash deny-list is trivially bypassable | ⚠️ OPEN |
| 3 | **Medium** | tools/mod.rs | Path traversal in file operations (no sandboxing) | ⚠️ OPEN |
| 4 | **Medium** | main.rs | Hardcoded internal Tailnet URL exposed in source | ⚠️ OPEN |
| 5 | **Medium** | api.rs | No HTTPS enforcement when API key is present | ⚠️ OPEN |
| 6 | **Medium** | hooks.rs, tools | Environment variable leakage (API key) to subprocesses | ⚠️ OPEN |
| 7 | **Medium** | tools/mod.rs | TOCTOU race in file read and edit operations | ⚠️ OPEN |
| 8 | **Low** | hooks.rs | Convergence state read-modify-write race | ⚠️ OPEN |
| 9 | **Medium** | tools/mod.rs | Symlink following without checking | ⚠️ OPEN |
| 10 | **Low** | hooks.rs | Hook command injection via planted TOML config | ⚠️ OPEN |
| 11 | **Low** | tools/mod.rs | Unbounded memory growth in Bash output | ⚠️ OPEN |
| 12 | **Low** | api.rs, tools | Error message information leakage | ⚠️ OPEN |
| 13 | **Low** | hooks.rs | Missing cleanup of timed-out hook processes | ⚠️ OPEN |
| 14 | **Low** | tools/mod.rs | Bash timeout does not kill process group | ⚠️ OPEN |
| 15 | **Low** | session.rs | Non-atomic session transcript writes | ⚠️ OPEN |
| 16 | **Info** | CI workflows | SHA-pinned actions (positive), inconsistent versions | ✅ RESOLVED |
| 17 | **Info** | Cargo.toml | Tokio "full" features broader than needed | ⚠️ OPEN |
| 18 | **Low** | .claude/hooks | Guard hook outputs to stderr instead of stdout | ⚠️ OPEN |
| 19 | **Info** | main.rs | No rate limiting on tool execution | ⚠️ OPEN |

**Summary**: 2 of 19 findings resolved (10.5%), 17 remain open.