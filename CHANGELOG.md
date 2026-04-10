# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/).

## [0.2.3] - 2026-04-10

### Fixed

- Misleading 404 on reserve/decide/event when the request unit does not match
  the stored budget's unit ([#8](https://github.com/runcycles/cycles-client-rust/issues/8)).
  The server indexes budgets by `(scope, unit)`, so reserving in the wrong
  unit surfaces as `"Budget not found for provided scope: …"` even when the
  scope itself has an ACTIVE budget. `create_reservation`,
  `create_reservation_with_metadata`, `decide`, and `create_event` now
  enrich such 404s in-flight with the unit that was sent, so the mismatch is
  self-diagnosing. No behavioral change for other errors.

### Docs

- `Amount`, `WithCyclesConfig::new`, the `with_cycles_usage` example, and the
  README Quick Start all note the `(scope, unit)` budget indexing invariant.

## [0.2.2] - 2026-04-02

### Fixed

- Removed `BlockingCyclesClient::builder()` which misleadingly returned a builder that produces an async client
- Added `CyclesClientBuilder::build_blocking()` to correctly build a `BlockingCyclesClient` from the shared builder

### Added

- `Amount::risk_points()` convenience constructor matching existing `usd_microcents()`, `tokens()`, and `credits()` constructors
- `SignedAmount` convenience constructors: `usd_microcents()`, `tokens()`, `credits()`, `risk_points()`
- `BlockingCyclesClient::config()` accessor for parity with async client
- `BlockingCyclesClient::create_reservation_with_metadata()` for accessing response headers in blocking mode

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
