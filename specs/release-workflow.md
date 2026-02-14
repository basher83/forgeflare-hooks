# GitHub Release Workflow Specification

**Status:** Active
**Target:** Cross-platform release binaries via GitHub Actions, triggered by version tags

---

## Requirements

**R1. Trigger**
- Run on version tag push (`v*`) matching existing tagging convention (v0.0.0 through v0.0.42+)
- No manual dispatch; tags are the single release mechanism

**R2. Build Matrix**
- Two targets in a matrix strategy:
  - `aarch64-apple-darwin` — build on `macos-latest` (Apple Silicon runner)
  - `x86_64-unknown-linux-gnu` — build on `ubuntu-latest`
- Each target compiles a release binary (`cargo build --release`)
- Each target runs the test suite before packaging to catch platform-specific failures

**R3. Artifact Packaging**
- Binary name: `forgeflare` (from `[[bin]]` in Cargo.toml)
- Tarball per target: `forgeflare-{tag}-{target}.tar.gz`
- Tarball contents: the `forgeflare` binary only (no docs, no config, no extras)
- Compression: gzip

**R4. GitHub Release**
- Create a GitHub Release from the tag after all matrix builds succeed
- Attach both tarballs as release assets
- Release title: the tag name (e.g., `v0.0.43`)
- Release body: auto-generated changelog from commits since previous tag (`gh release create --generate-notes` or equivalent)
- Mark as latest release

**R5. CI Gate**
- Release job depends on existing CI jobs (lint, audit, test, build) passing
- The release workflow either triggers CI as a prerequisite or the release workflow file includes the CI validation steps inline per matrix leg
- No release artifacts are published if any validation step fails

**R6. Consistency with Existing CI**
- Pin all action SHAs (same convention as `ci.yml`)
- Use `jdx/mise-action` for tool orchestration where applicable
- Use `Swatinem/rust-cache` for build caching
- Least-privilege permissions: `contents: write` only on the release creation job, `contents: read` on build jobs

**R7. Cross-Compilation**
- macOS aarch64: native compilation on Apple Silicon runner (no cross-compile needed)
- Linux x86_64: native compilation on Ubuntu runner (no cross-compile needed)
- No cross-compilation toolchain required for this target set

---

## Architecture

```yaml
on: push tags v*
  │
  ├─ CI gate (lint, audit, test) ─── must pass
  │
  ├─ build matrix
  │   ├─ macos-latest ──→ cargo test ──→ cargo build --release ──→ tar.gz
  │   └─ ubuntu-latest ─→ cargo test ──→ cargo build --release ──→ tar.gz
  │
  └─ release (needs: [ci, build])
      └─ gh release create $TAG --generate-notes ──→ attach tarballs
```

File: `.github/workflows/release.yml` (separate from `ci.yml`)

---

## Success Criteria

- [ ] Tag push triggers release workflow
- [ ] macOS aarch64 binary compiles and tests pass on Apple Silicon runner
- [ ] Linux x86_64 binary compiles and tests pass on Ubuntu runner
- [ ] Both tarballs attached to GitHub Release
- [ ] Tarball extracts to a working `agent` binary on target platform
- [ ] Release blocked if CI (lint, audit, test) fails
- [ ] No release artifacts published on build failure
- [ ] Action SHAs pinned, permissions least-privilege

---

## Non-Goals

- Intel Mac (x86_64-apple-darwin) builds
- ARM Linux (aarch64-unknown-linux-gnu) builds
- `.deb` or `.rpm` packaging
- Homebrew formula generation
- Automatic version bumping or changelog files
- Docker image builds
- Code signing or notarization
- Manual workflow_dispatch trigger

---

## Implementation Notes

- Both targets are native builds on their respective runners — no cross-compilation toolchain, no `cross` crate, no `cargo-zigbuild`. This is the simplest possible setup.
- The `macos-latest` GitHub runner is Apple Silicon (M-series) as of 2024. If GitHub changes this, the workflow breaks visibly (wrong arch in binary) rather than silently.
- Tarball naming uses the Rust target triple for unambiguous platform identification. Users `tar xzf` and get the binary.
- The release job needs `contents: write` to create releases. Build jobs stay at `contents: read`.
- Auto-generated release notes from `gh release create --generate-notes` are sufficient. No custom changelog tooling needed for a research project at v0.0.x.
- Static linking: default Rust Linux builds link glibc dynamically. This is fine for Ubuntu/Debian targets. If musl static linking is needed later, that's a separate spec change (adds `x86_64-unknown-linux-musl` target).
