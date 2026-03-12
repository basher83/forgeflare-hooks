---
status: Complete
created: 2026-03-11
---

# Load Project Instructions into System Prompt

**Target:** Read `CLAUDE.md` or `AGENTS.md` at startup and append contents to the system prompt, giving the agent project-specific operational context

---

## Why

`build_system_prompt()` in `src/main.rs` returns a hardcoded string with generic tool descriptions and guidelines. It has no awareness of the project it's operating in. Project instruction files contain build commands, code patterns, and operational notes that the agent needs to follow.

Two conventions exist for these files: `CLAUDE.md` (Claude Code's convention) and `AGENTS.md` (the Ralph/forge convention, sometimes symlinked as `CLAUDE.md`). A project may have either, both, or a symlink between them. ForgeFlare should handle all cases by trying both filenames with a defined priority order.

---

## Requirements

**R1. Read Project Instructions at Startup**
- After constructing the base system prompt, search for project instruction files in the current working directory
- Try files in this order: `CLAUDE.md` first, then `AGENTS.md`
- Use the FIRST file found. Do not load both — if `CLAUDE.md` exists, skip `AGENTS.md` entirely (they're often the same content via symlink, and loading both would duplicate instructions)
- If neither file exists, skip silently (no error, no warning)
- If a file exists but is unreadable (permissions), log a warning to stderr and try the next candidate
- Follow symlinks — `CLAUDE.md -> AGENTS.md` and the reverse both resolve naturally

**R2. Prompt Integration**
- Append project instructions after the base system prompt, separated by a clear delimiter
- The section header names the actual file that was loaded:

```text
{base system prompt}

---

## Project Instructions (from {filename})

{contents}
```

- No truncation — include the full file contents. These files should be kept short by convention (~60 lines per AGENTS.md guide). If someone puts a 50KB file there, that's their problem.

**R3. Size Guard**
- Skip loading if the file exceeds 32KB (32,768 bytes). Log a warning: `[warn] {filename} exceeds 32KB, skipping`
- If skipped for size, try the next candidate in the search order
- This prevents accidental context pollution from a misconfigured symlink pointing at a large file

**R4. Verbose Logging**
- In verbose mode, log: `[verbose] Loaded project instructions from {filename} ({n} bytes)`
- If skipped (neither found), log: `[verbose] No CLAUDE.md or AGENTS.md found in working directory`

---

## Architecture

```text
main()
  │
  ├─ build_system_prompt()  →  base prompt (unchanged)
  │
  ├─ load_project_instructions()  →  InstructionsResult
  │    │   enum InstructionsResult {
  │    │       Found { filename: String, contents: String },
  │    │       Skipped { filename: String, reason: String },
  │    │       NotFound,
  │    │   }
  │    ├─ for candidate in ["CLAUDE.md", "AGENTS.md"]:
  │    │    ├─ metadata() fails → continue (file not found or dangling symlink)
  │    │    ├─ metadata().len() > 32KB → yield Skipped, continue to next candidate
  │    │    ├─ read_to_string() fails → yield Skipped (permission denied, I/O error), continue
  │    │    └─ success → return Found { candidate, contents }
  │    └─ if loop exhausts → return last Skipped if any candidate was skipped, otherwise NotFound
  │
  ├─ match result:
  │    ├─ Found { filename, contents }:
  │    │    ├─ if verbose: eprintln!("[verbose] Loaded project instructions from {filename} ({n} bytes)")
  │    │    └─ system_prompt = format!("{base}\n\n---\n\n## Project Instructions (from {filename})\n\n{contents}")
  │    ├─ Skipped { filename, reason }:
  │    │    └─ eprintln!("[warn] {filename}: {reason}")  // always warn, not verbose-gated
  │    └─ NotFound:
  │         └─ if verbose: eprintln!("[verbose] No CLAUDE.md or AGENTS.md found in working directory")
  │
  └─ pass system_prompt to run_turn (unchanged interface)
```

Changes to existing code:

1. `src/main.rs` — Add `enum InstructionsResult` and `fn load_project_instructions() -> InstructionsResult`. Tries `CLAUDE.md` then `AGENTS.md` from cwd with size guard and read error handling. The function does no logging — it returns structured data. The caller in `main()` handles logging (verbose-gated for Found/NotFound, always-on for Skipped warnings) and prompt assembly.

---

## Success Criteria

- [ ] Agent receives project instructions in system prompt when CLAUDE.md exists
- [ ] Agent receives project instructions in system prompt when only AGENTS.md exists (no CLAUDE.md)
- [ ] CLAUDE.md takes priority over AGENTS.md when both exist
- [ ] Symlinked files (either direction) work correctly
- [ ] Neither file present (metadata fails for both) produces no error or warning
- [ ] File over 32KB is skipped with a warning, falls through to next candidate
- [ ] File present but unreadable (permission denied) produces a warning and falls through to next candidate
- [ ] Both candidates skipped (any combination of size/permission) produces a warning; agent runs without project instructions
- [ ] Verbose mode logs which file was loaded (by name)
- [ ] System prompt section header names the actual file loaded
- [ ] No change to `build_system_prompt()` function signature
- [ ] All existing tests pass

---

## Non-Goals

- Recursive parent directory traversal (Claude Code walks up the directory tree; we read only from cwd)
- Loading `.claude/` directory settings or rules files
- Hot-reloading instructions during a session (read once at startup)
- Caching the file contents separately from the system prompt (the prompt caching spec handles API-level caching)
- User-configurable instruction file paths or search order (always CLAUDE.md then AGENTS.md)
- Merging both files if both exist (first match wins)

---

## Implementation Notes

- The function performs no logging — it returns an `InstructionsResult` enum and the caller in `main()` handles all output. This keeps the function testable and gives the caller full control over verbose vs. always-on warnings.
- The `Skipped` variant captures both the filename and reason string (e.g., "exceeds 32KB", "permission denied: ..."). If both candidates are skipped, the function returns the last `Skipped` result so the caller can warn about it — the first skip's warning is silently dropped. This is a deliberate simplicity tradeoff: most projects have one instruction file, and the double-skip case is rare enough to not warrant collecting all reasons into a Vec. If neither candidate exists (metadata fails with NotFound for both), it returns `NotFound`.
- The search is a simple loop over `["CLAUDE.md", "AGENTS.md"]`. For each candidate, check `std::fs::metadata()` for existence and size, then `std::fs::read_to_string()` for contents. Both follow symlinks by default.
- When `CLAUDE.md` is a symlink to `AGENTS.md`, `metadata("CLAUDE.md")` resolves the symlink and reads the target's metadata. `read_to_string("CLAUDE.md")` reads the target's contents. The symlink case requires zero special handling. Dangling symlinks cause `metadata()` to fail with `NotFound`, which is handled by the loop's continue path.
- When both `CLAUDE.md` and `AGENTS.md` exist as separate files with different content, only `CLAUDE.md` is loaded. This is intentional — if someone maintains both, they should be aware that the agent sees only one.
- This interacts with the prompt caching spec: the system prompt grows when instructions are loaded, making caching even more valuable. The `send_message` function continues to accept the system prompt as `&str` and wraps it into content blocks internally, so the string concatenation approach in `main()` is compatible regardless of implementation order.
- The 32KB limit is conservative. A typical AGENTS.md is 500-2000 bytes. 32KB would be ~8000 tokens of context consumed per API call, which is significant but not catastrophic with caching enabled.
