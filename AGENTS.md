## Build & Run

Single Rust binary. `cargo build` to compile, `cargo run` to run.

On the tailnet (default): `cargo run` — no env vars needed, OAuth proxy handles auth.
Off tailnet: `ANTHROPIC_API_KEY=sk-... cargo run -- --api-url https://api.anthropic.com`

## Validation

Run these after implementing to get immediate feedback:

- Tests: `cargo test`
- Typecheck: `cargo build` (Rust compiler is the type checker)
- Lint: `cargo clippy -- -D warnings`
- Format: `cargo fmt --check`
- Full validation: `cargo fmt --check && cargo clippy -- -D warnings && cargo test`

## Operational Notes

Project structure: `src/main.rs`, `src/api.rs`, `src/tools/mod.rs`, `src/hooks.rs`, `src/session.rs`
Hook config: `.forgeflare/hooks.toml`
Convergence state: `.forgeflare/convergence.json`
Session transcripts: `.entire/metadata/{session-id}/`

### Codebase Patterns

- Tool names are PascalCase: Read, Glob, Bash, Edit, Grep (Claude Code conventions for OAuth proxy compatibility)
- `tools!` macro generates schemas; `dispatch_tool()` is hand-written
- `dispatch_tool` is sync; async only for HTTP and hook subprocess execution
- Hooks are shell executables: JSON stdin, JSON stdout, stderr forwarded
- Guard hooks are fail-closed; observe/post/stop hooks are fail-open
- Convergence writes are atomic (temp file + rename in same directory)
