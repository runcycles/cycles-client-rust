## Git Rules — STRICT
- ALWAYS use native git for ALL commits and pushes
- NEVER use mcp__github__ tools for committing or pushing
- Use mcp__github__ ONLY for: PRs, Issues, GitHub Actions
- Write commit messages to a temp file, then: `git commit -F <file>`
- NEVER use --no-gpg-sign flag

# Cycles strict rules
- yaml API specs always the authority
- always update AUDIT.md files when making changes to server, admin, client repos
- maintain at least 95% or higher test coverage for all code repos

# Build & Test
- Build: `cargo build`
- Test: `cargo test`
- Test with coverage: `cargo tarpaulin --skip-clean --out Stdout --ignore-tests -- --skip live`
- Clippy: `cargo clippy -- -W clippy::all`
- Format: `cargo fmt --check`
- Live server tests (requires running server): `cargo test --test live_server_test -- --ignored`
