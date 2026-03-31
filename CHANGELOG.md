# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/).

## [0.2.1] - 2026-03-31

### Fixed

- README version reference (was 0.1, now 0.2)
- Outdated rustdoc on `with_cycles()` referencing `ReservationGuard` instead of `GuardContext`
- CI: MSRV bumped from 1.75 to 1.88 (transitive deps require edition 2024)
- CI: clippy `map_or` → `is_none_or` for Rust 1.94+ stable

## [0.2.0] - 2026-03-31

### Added

- Initial release of the Cycles Rust client
- `CyclesClient` with all 9 protocol endpoints (reserve, commit, release, extend, decide, events, list, get, balances)
- `ReservationGuard` RAII type with ownership-based compile-time safety
- `with_cycles()` automatic lifecycle wrapper (like Python's `@cycles` decorator / TypeScript's `withCycles`)
- `GuardContext` for accessing decision, caps, reservation ID inside `with_cycles` closures
- Three integration levels: `with_cycles()` (automatic), `ReservationGuard` (manual RAII), low-level client API
- Automatic heartbeat (TTL extension) via background tokio task
- Commit retry engine with exponential backoff
- Newtype IDs (`ReservationId`, `IdempotencyKey`, `EventId`)
- `#[non_exhaustive]` enums with `#[serde(other)]` for forward compatibility
- `bon::Builder` for request construction with compile-time required field enforcement
- `CyclesConfig` with environment variable loading and builder
- `ApiResponse<T>` wrapper for accessing rate limit headers
- Blocking client behind `blocking` feature flag
- Input validation for subjects, TTL, grace periods, amounts
- GitHub Actions CI workflow (shared reusable `ci-rust.yml`)
- 95%+ test coverage (141 tests)
