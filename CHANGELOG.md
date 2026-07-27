# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/).

## [0.3.0] - 2026-07-27

Commit durability: expired-commit event fallback, plus `Retry-After`-aware retry.

### Added

- **Event fallback on `RESERVATION_EXPIRED` commits.** A commit records spend that already happened, so when the commit lands after the reservation's grace period — the server has already returned the reserved budget to the pool — the spend must not be silently dropped. `guard.commit(...)` now recovers it as a post-hoc direct-debit event (`POST /v1/events`, previously implemented in `CyclesClient::create_event` but never called by the lifecycle):
  - the event reuses the commit's **idempotency key**, so the recovery is exactly-once across the separate event namespace;
  - subject and action come from the reservation (the guard now retains them; see below), `actual` from the commit;
  - the commit's metadata is carried over, extended with `recovered_reservation_id` (the reservation's ID) and `recovery_reason = "commit_after_reservation_expired"` so the ledger shows why the spend arrived as an event;
  - no `overage_policy` is set — the server default `ALLOW_IF_AVAILABLE` never rejects, which is correct for spend that already happened;
  - transient event failures (transport, 5xx) are retried with the same bounded backoff policy as commits, reusing the same request.

  On fallback success `commit()` returns `Ok` with the new client-side `CommitStatus::RecoveredViaEvent` and `CommitResponse::recovered_via_event: Option<EventId>` set (plus `CommitResponse::is_recovered_via_event()`); both are additive on `#[non_exhaustive]` types, so existing callers keep compiling and a plain `commit(...).await?` keeps meaning "spend recorded". `RESERVATION_FINALIZED` deliberately does **not** trigger the fallback — the reservation was already committed or released, so no spend is lost.
- `Error::CommitRecoveryFailed { reservation_id, commit_error, event_error }` — returned when the event fallback fails too. Carries **both** underlying errors (why the commit expired and why recovery failed), is non-retryable by construction, and its `Display` states plainly that the spend is NOT recorded.
- `ReservationGuard::subject()` / `::action()` accessors — the guard now retains the reservation's subject and action (threaded through from `CyclesClient::reserve`), needed by the fallback and useful for callers.
- `Error::is_auth_error()` — `true` for HTTP 401/`UNAUTHORIZED` and 403/`FORBIDDEN`. These stay non-retryable by design (retrying the same credentials cannot succeed; the truthful `Err` lets the caller rotate keys or fix permissions); the helper makes them programmatically distinct.

### Changed

- **`Retry-After` is parsed and honored.** Error responses now populate `Error::retry_after()` from the `Retry-After` header (delta-seconds form; previously always `None`), and the retry loop waits **at least** the server's advertised delay after an HTTP 429 `LIMIT_EXCEEDED` — even when that exceeds `retry_max_delay`. Each response's `Retry-After` is consumed exactly once (it governs only the sleep immediately following it); plain exponential backoff applies otherwise.

### Notes

- **Source-level breakage:** `Error` is not `#[non_exhaustive]`, so downstream `match`es over all `Error` variants must add a `CommitRecoveryFailed` arm — hence the 0.3.0 minor bump. Everything else is additive (`CommitStatus` and `CommitResponse` are `#[non_exhaustive]`; `ReservationGuard::new` is `pub(crate)`).
- Regression tests: `tests/recovery_test.rs` (expired→event recovery success incl. idempotency-key reuse and metadata markers, recovery-failure surfacing both errors, bounded event retry, `RESERVATION_FINALIZED` non-recovery, 429 `Retry-After` wall-clock honor, header parsing), plus unit tests for the delay policy (`src/retry.rs`), the new error variant/helper (`tests/error_test.rs`), and the `RECOVERED_VIA_EVENT` serde roundtrip (`src/models/enums.rs`).

## [0.2.7] - 2026-07-17

Commit-retry wiring fix, plus `TENANT_CLOSED` + `LIMIT_EXCEEDED` error-code support.

### Fixed

- **Commit retry engine wired into `guard.commit()`** ([#62](https://github.com/runcycles/cycles-client-rust/pull/62)). `CommitRetryEngine` existed since 0.2.0 with complete backoff logic but was `#[allow(dead_code)]` and never invoked, so the documented `retry_*` config knobs (builder methods, `CYCLES_RETRY_*` env vars) were silent no-ops and a transient commit failure permanently leaked the reservation until server-side TTL expiry, never recording the actual spend. `guard.commit()` now retries retryable failures (transport errors, 5xx, and error codes the protocol classifies transient per `Error::is_retryable`) **inline** with exponential backoff:
  - the heartbeat keeps extending the reservation TTL until the commit outcome is final, so retries cannot lose the reservation to expiry;
  - the returned `Ok`/`Err` is definitive — no commit activity survives the call, so callers may safely compensate on `Err`;
  - no detached background task exists, so nothing is silently lost on runtime shutdown;
  - retries reuse the original `CommitRequest` (same idempotency key), so a commit that already landed server-side cannot double-charge.

  Under a persistent outage `commit()` blocks for the full backoff schedule; tune the `retry_*` knobs or set `retry_enabled(false)` for fail-fast single-attempt commits.
- `Cargo.lock`: `quinn-proto` 0.11.14 → 0.11.16 for [RUSTSEC-2026-0185](https://rustsec.org/advisories/RUSTSEC-2026-0185) (remote memory exhaustion via unbounded out-of-order stream reassembly) and `anyhow` 1.0.102 → 1.0.103 for [RUSTSEC-2026-0190](https://rustsec.org/advisories/RUSTSEC-2026-0190) (unsoundness in `Error::downcast_mut()`); both transitive dependencies, flagged by the `cargo audit --deny warnings` CI gate.

`TENANT_CLOSED` + `LIMIT_EXCEEDED` error-code support. `TENANT_CLOSED` implements the runtime spec v0.1.25.13 revision of `cycles-protocol-v0.yaml` ([runcycles/cycles-protocol#125](https://github.com/runcycles/cycles-protocol/pull/125)): servers return HTTP 409 `error=TENANT_CLOSED` on reservation create/commit/release/extend when the owning tenant is CLOSED (mirrors governance spec Rule 2). `LIMIT_EXCEEDED` closes the same class of gap for the runtime spec v0.1.25.12 revision (2026-07-04): HTTP 429 rate-limit responses (public evidence/JWKS endpoints) carry `error=LIMIT_EXCEEDED` plus `Retry-After` / `X-RateLimit-Reset` headers.

### Added

- `ErrorCode::TenantClosed` variant (serde string mapping `"TENANT_CLOSED"`). `ErrorCode` is `#[non_exhaustive]` with a `#[serde(other)] Unknown` arm, so this is source- and wire-compatible.
- `Error::is_tenant_closed()` helper, mirroring `Error::is_budget_exceeded()`: matches `Error::Api { code: Some(ErrorCode::TenantClosed), .. }`.
- Regression tests: serde roundtrip for the new variant (`src/models/enums.rs`), `Error` helper behavior (`tests/error_test.rs`), and a wiremock test pinning that a 409 `TENANT_CLOSED` body surfaces as `Error::Api` with the typed code — not the `BudgetExceeded` convenience variant — and is non-retryable (`tests/client_test.rs`).
- `CyclesClientBuilder::retry_initial_delay()`, `::retry_multiplier()`, `::retry_max_delay()` — the retry knobs that previously existed only as config fields / env vars now have builder setters ([#62](https://github.com/runcycles/cycles-client-rust/pull/62)).
- `ErrorCode::LimitExceeded` variant (serde string `"LIMIT_EXCEEDED"`), added in spec declaration order (after `MaxExtensionsExceeded`; `TenantClosed` relocated after it so the enum mirrors the spec exactly). Classified **retryable** by `ErrorCode::is_retryable()` — 429 is transient and the spec instructs retry after the indicated delay; `Error::is_retryable()` picks this up via the code-based arm (the status-based arm only covers ≥500). This preserves the prior `#[serde(other)] Unknown → retryable` fallback behavior, now typed instead of accidental. Enum-only by design, matching the `BudgetFrozen`/`BudgetClosed` pattern: not a reservation-lifecycle denial, so no `Error` helper or 409 classification change. Serde roundtrip + `Error` retryability + wiremock 429 regression tests added.

### Notes

- Purely additive; no wire-format change. Before this release, a server returning `TENANT_CLOSED` deserialized to `ErrorCode::Unknown` via the `#[serde(other)]` forward-compat arm — deserialization never failed, but `ErrorCode::Unknown.is_retryable()` is `true`, so `Error::is_retryable()` reported a 409 TENANT_CLOSED as retryable. With this release the code is typed and correctly non-retryable.
- The 409 classification in `client.rs` (BUDGET_EXCEEDED / OVERDRAFT_LIMIT_EXCEEDED / DEBT_OUTSTANDING → `Error::BudgetExceeded`) is intentionally unchanged: TENANT_CLOSED is a tenant-state error, not a budget-family error, and surfaces as `Error::Api`.

## [0.2.6] - 2026-05-22

`expires_*` / `finalized_*` ISO-8601 window-filter fields on `ListReservationsParams`, plus optional `finalized_at_ms` on `ReservationSummary`. Implements `cycles-protocol-v0.yaml` revision 2026-05-22 ([runcycles/cycles-protocol#98](https://github.com/runcycles/cycles-protocol/pull/98)) on the client side; runcycles/cycles-server#163 ships the server impl. Closes the Rust-client side of runcycles/cycles-server#162.

### Added

- `ListReservationsParams::expires_from`, `::expires_to`, `::finalized_from`, `::finalized_to` (`Option<String>`, ISO 8601 date-time). Each pair binds to its target field (`expires_at_ms`, `finalized_at_ms`) independent of `from`/`to` and of any `sort_by`. The three windows compose with AND semantics. `finalized_*` excludes ACTIVE and EXPIRED rows per the spec (field absent → predicate fails).
- `ReservationSummary::finalized_at_ms` (`Option<u64>`). Populated by servers on COMMITTED and RELEASED rows; absent (deserialized as `None`) on ACTIVE/EXPIRED and on pre-v0.1.25.21 servers regardless of status. `#[serde(default)]` keeps deserialization back-compatible.
- Three regression tests under `tests/client_test.rs`:
  - `list_reservations_forwards_expires_and_finalized_windows`: wiremock `query_param` matchers assert all four new fields land on the wire under their spec-mandated names.
  - `list_reservations_deserializes_finalized_at_ms_on_summary`: confirms the field deserializes to `Some(value)` when the server emits it.
  - `list_reservations_deserializes_absent_finalized_at_ms_as_none`: confirms back-compat with servers that don't emit the field.

### Notes

- Pure additive struct change for callers using `ListReservationsParams::default()` or `..Default::default()`.
- **Source-level breakage for exhaustive constructors.** `ListReservationsParams` is not `#[non_exhaustive]`, so downstream callers who construct it field-by-field will need to add `expires_from: None, expires_to: None, finalized_from: None, finalized_to: None` or switch to `..Default::default()`. Mirrors the v0.2.5 additive bump.
- `ReservationSummary` is `#[non_exhaustive]` (and `Deserialize`-only — callers can't construct it directly), so the new field is fully transparent.
- 134 tests pass across the integration + unit suites; doc-tests + clippy clean.

## [0.2.5] - 2026-05-21

`from` / `to` ISO-8601 window-filter fields on `ListReservationsParams`. Implements `cycles-protocol-v0.yaml` revision 2026-05-21 ([runcycles/cycles-protocol#97](https://github.com/runcycles/cycles-protocol/pull/97)) on the client side; runcycles/cycles-server#160 ships the server impl. Closes the Rust-client side of runcycles/cycles-server#159.

### Added

- `ListReservationsParams::from` and `::to` (`Option<String>`, ISO 8601 date-time). Both are inclusive bounds on `created_at_ms`. Either may be supplied alone (open interval) or together (closed window). The filter binds to `created_at_ms` regardless of any sort key. Servers reject `from > to` with HTTP 400 `INVALID_REQUEST`.
- Regression test `list_reservations_forwards_from_to_window` in `tests/client_test.rs` using wiremock `query_param` matchers to assert that the new fields land on the wire under the spec-mandated query-string names.

### Notes

- Pure additive struct change for callers using `ListReservationsParams::default()` or struct-update syntax `..Default::default()` — the new fields default to `None` and serialize as absent.
- **Source-level breakage for exhaustive constructors.** `ListReservationsParams` is not `#[non_exhaustive]`, so downstream callers who construct it field-by-field without `..Default::default()` (e.g. `let p = ListReservationsParams { status, tenant, app, agent, cursor, limit };`) will need to add `from: None, to: None` or switch to the `..Default::default()` shape. Mirrors the previous additive bumps to this struct.
- 134 tests pass across the integration + unit suites; doc-tests + clippy clean.

## [0.2.4] - 2026-05-08

### Changed

- Crates.io description and keywords broadened to cover the three pillars of
  Cycles' runtime authority: spend, risky tool actions, and audit gaps. Prior
  framing ("budget-management protocol — deterministic spend control") only
  surfaced the spend dimension and missed search-intent traffic for action
  control and audit-trail use cases.
- Keyword set updated from `["cycles", "budget", "llm", "ai-agents",
  "cost-control"]` to `["ai-agents", "llm", "budget", "governance",
  "audit-log"]`. Same five-keyword cap, broader coverage.
- `README.md` opening reorganized around the three pillars (spend / risky
  actions / audit gaps), each with a one-line concrete affordance, instead
  of leading with budget enforcement only.

No behavioral changes. API surface, wire protocol, and conformance audit
results are identical to 0.2.3.

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
