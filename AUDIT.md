# Protocol Conformance Audit — Rust Client

- **Date:** 2026-07-17 (v0.2.7 — commit-retry wiring fix: `CommitRetryEngine` (`src/retry.rs`) existed with complete backoff logic but carried `#[allow(dead_code)]` and was never instantiated outside its own unit tests, so the documented `retry_*` config knobs (builder methods, `CYCLES_RETRY_*` env vars, README) were silent no-ops — and a transient commit failure permanently leaked the reservation until server-side TTL expiry, because `guard.commit()` sets `finalized = true` and consumes the guard before the network call, so `Drop` performs no best-effort release and the caller cannot retry. `ReservationGuard::commit` now retries retryable failures (transport errors, 5xx, error codes the protocol classifies transient per `Error::is_retryable`, incl. the `Unknown` forward-compat arm) **inline** with exponential backoff — the fire-and-forget design originally sketched in the dead code was rejected in adversarial review because it (a) let the retry window outlive the cancelled heartbeat and die on `RESERVATION_EXPIRED`, (b) broke the "commit `Err` is final" invariant, enabling double-charge via caller compensation racing a late background commit, and (c) silently lost pending retries on runtime shutdown (detached `tokio::spawn`). Inline semantics restore all three properties: the heartbeat now stays alive until the commit outcome is final (cancelled after, with `Drop` as backstop), `Ok`/`Err` from `commit()` is definitive, and no detached task exists. Retries reuse the original `CommitRequest` — same idempotency key — so a commit that already landed server-side cannot double-charge. Both `#[allow(dead_code)]` attributes removed; `CyclesClientBuilder` gained the three missing retry setters (`retry_initial_delay`, `retry_multiplier`, `retry_max_delay`). `tests/retry_test.rs` — previously titled "Tests for CommitRetryEngine" while never exercising the engine — rewritten as four end-to-end reserve→commit wiremock tests (retry-until-success with idempotency-key-reuse and upper-bound `.expect` assertions, exhaustion, non-retryable-final, retry-disabled-final); shared reserve-mock scaffolding extracted to `tests/common/mod.rs` (also used by `guard_test.rs`). README documents the retry semantics. Coverage 96.12%. Follow-up same day: `Cargo.lock` `quinn-proto` 0.11.14 → 0.11.16 for RUSTSEC-2026-0185 (remote memory exhaustion; flagged by the scheduled cargo-audit run of 2026-07-13) and `anyhow` 1.0.102 → 1.0.103 for RUSTSEC-2026-0190 (`Error::downcast_mut()` unsoundness; the CI gate runs `cargo audit --deny warnings`, so unsound-warnings fail too). Both transitive dependencies.), 2026-07-10 (v0.2.7 — `TENANT_CLOSED` error-code support per runtime spec v0.1.25.13 (`cycles-protocol-v0.yaml`, runcycles/cycles-protocol#125): `ErrorCode::TenantClosed` variant with serde string mapping `"TENANT_CLOSED"`, plus `Error::is_tenant_closed()` helper mirroring `is_budget_exceeded()`. Purely additive — previously the code hit the `#[serde(other)] Unknown` forward-compat arm, which deserialized cleanly but reported the 409 as retryable via `ErrorCode::Unknown.is_retryable()`; now typed and non-retryable. The 409→`Error::BudgetExceeded` classification is intentionally unchanged (TENANT_CLOSED is tenant-state, not budget-family; it surfaces as `Error::Api`). Serde roundtrip + `Error` helper + wiremock regression tests added. Also `LIMIT_EXCEEDED` per runtime spec v0.1.25.12 (revision 2026-07-04, HTTP 429 rate limiting on the public evidence/JWKS endpoints, `Retry-After` / `X-RateLimit-Reset` headers): `ErrorCode::LimitExceeded` variant added in spec declaration order (`TenantClosed` relocated after it to mirror the spec exactly), classified retryable by `ErrorCode::is_retryable()` — 429 is transient; `Error::is_retryable()` inherits this via the code arm, preserving the prior `Unknown → retryable` fallback semantics, now typed. Enum-only, matching the `BudgetFrozen`/`BudgetClosed` sibling pattern (no `Error` helper, no 409-classification change). Serde roundtrip + retryability + wiremock 429 tests added.), 2026-07-04 (v0.2.7 — `reserve()` no longer panics on additive `Decision` values (fleet audit, #56 item 1): an unknown decision deserializes to `Decision::Unknown` via `#[serde(other)]`, bypassed `is_denied()`, and hit `.expect("reservation_id must be present…")`. Unknown/additive decisions now return `Error::Validation` regardless of `reservation_id` presence — `reserve()` gates on positive `Decision::is_allowed()`, not merely non-denial (review follow-up on the first cut, which still built a guard when an id happened to be present). Wiremock regression test added; full suite green. Remaining audit findings tracked in #56.), 2026-05-22 (v0.2.6 — `expires_*` / `finalized_*` ISO-8601 window-filter fields added to `ListReservationsParams` plus optional `finalized_at_ms` field added to `ReservationSummary` per `cycles-protocol-v0.yaml` revision 2026-05-22 (runcycles/cycles-protocol#98); closes the Rust-client side of runcycles/cycles-server#162. Four new `Option<String>` fields on the params struct (`expires_from`, `expires_to`, `finalized_from`, `finalized_to`), one new `Option<u64>` field on the response struct (`finalized_at_ms`, with `#[serde(default)]` for back-compat with pre-v0.1.25.21 servers). Wire-format regression tests + finalized_at_ms deserialization tests added. 134 tests pass; clippy + doc-tests clean.), 2026-05-21 (v0.2.5 — `from` / `to` ISO-8601 window-filter fields added to `ListReservationsParams` per `cycles-protocol-v0.yaml` revision 2026-05-21; closes the Rust-client side of runcycles/cycles-server#159. Both `Option<String>`, both inclusive bounds on `created_at_ms`, both serialize via `#[serde(rename = "...")]` to land on the wire under the spec-mandated names. Pure additive struct change — callers using `Default::default()` or struct-update syntax stay compile-clean. Wire-format regression test added using wiremock's `query_param` matcher. 134 tests pass; clippy + doc-tests clean.), 2026-04-10 (protocol conformance), 2026-04-19 (supply-chain coverage — cargo-audit workflow added), 2026-05-08 (crates.io metadata refresh — description and keywords broadened to cover spend / risk / audit, no behavioral changes)
- **Spec:** `cycles-protocol-v0.yaml` v0.1.25 (OpenAPI 3.1.0)
- **Client:** Rust 1.88+ (MSRV), reqwest 0.12, serde 1, tokio 1, bon 3
- **Cross-reference:** [cycles-server AUDIT.md](https://github.com/runcycles/cycles-server/blob/main/AUDIT.md)
- **Supply-chain coverage:** `.github/workflows/cargo-audit.yml` runs `cargo audit` against rustsec/advisory-db on PRs touching `Cargo.lock` / `Cargo.toml`, on push to `main`, and weekly (Monday 06:00 UTC). Fills the gap left by CodeQL default-setup, which has no Rust analyzer.

---

## 2026-07-27 — heartbeat extend-drift fix, lead-estimate redesign (v0.3.1)

P1 liveness, fleet-wide (same bug in all four SDKs). The spec's `extend_by_ms`
is relative to the reservation's *current* `expires_at_ms`, not request time,
but `src/heartbeat.rs` extended by `ttl_ms` on every `ttl/2` beat — drifting
the expiry outward `ttl/2` per beat (kill the process and the reserved budget
stays locked until the drifted expiry, up to ~6×ttl at default
`max_extensions`) and burning extensions twice as fast as needed. First cut
(alternate-beat cadence) was replaced after adversarial self-review found
confirmed liveness regressions: single-failure retry at zero lead; guaranteed
lapse for spec-legal ttl in (1000, 2000) under the 1 s interval floor;
sleep-after-await beat slip; non-`ACTIVE` 2xx treated as failure (every-beat
drift against any newer server); permanent failures retried forever; fresh
idempotency key per retry risking double-extend after a lost response. Fixed
with a **lead-estimate** scheduler: interval exactly `ttl/2` (floor removed),
beats anchored via `interval_at` + `MissedTickBehavior::Skip`; per beat
`lead = (known_expiry − initial_expiry) + ttl − elapsed` (server-frame minus
server-frame, monotonic minus monotonic — never client vs server clock;
reserve's `expires_at_ms` threaded from the guard); skip iff `lead ≥ 1.5·ttl`,
else extend by `ttl_ms`. Any 2xx counts as applied (`expires_at_ms`
authoritative, warn on odd status — reverses the first cut); transient
failures reuse the same idempotency key next beat (replay dedupe); permanent
codes (`RESERVATION_EXPIRED`/`RESERVATION_FINALIZED`/`MAX_EXTENSIONS_EXCEEDED`
or HTTP 410) stop the heartbeat. Cancellation unchanged. Six wiremock tests
with dynamic expiry responders + lead-math/classification unit tests
(`tests/heartbeat_test.rs`, `src/heartbeat.rs`). Coverage 95.70%; tests,
clippy `-D warnings`, fmt green.

## 2026-07-27 — v0.3.0 self-review hardening

Adversarial review of the fallback PR: bodyless 429s retry by status alone
(honoring `Retry-After`, now clamped to 1 hour per fleet decision D2); an
HTTP 410 with a mangled body still triggers event recovery (new
`Error::status()`); the heartbeat is cancelled before recovery runs; a
non-`APPLIED` fallback-event status is recovery *failure*; the wire string
`"RECOVERED_VIA_EVENT"` deserializes to `Unknown` so a server cannot fabricate
a recovery; `BudgetExceeded` is retryable only from a real 429 (new `status`
field). Coverage 95.28%; tests, clippy `-D warnings`, fmt green.

## 2026-07-27 — expired-commit event fallback + Retry-After (v0.3.0)

A commit records spend that already happened, so `guard.commit()` no longer
drops it when the reservation expired before the commit landed: the spend is
recovered as a `POST /v1/events` direct-debit reusing the commit's
idempotency key (exactly-once across the event namespace), with
`recovered_reservation_id` / `recovery_reason` metadata markers and no
overage policy (server default `ALLOW_IF_AVAILABLE`); surfaced as
`CommitStatus::RecoveredViaEvent` + `CommitResponse::recovered_via_event`
(additive on `#[non_exhaustive]` types). A failed fallback returns the new
`Error::CommitRecoveryFailed` carrying both the expired-commit error and the
event error. `RESERVATION_FINALIZED` intentionally does not trigger recovery.
Error responses now parse the `Retry-After` header (previously dropped) and
the retry loop waits at least the server's delay on 429 `LIMIT_EXCEEDED`,
consumed once per response. Added `Error::is_auth_error()`; 401/403 stay
truthfully non-retryable. Seven wiremock e2e tests (`tests/recovery_test.rs`)
plus unit coverage in `src/retry.rs`, `tests/error_test.rs`, and the enum
serde suite; full test suite, clippy `-D warnings`, and fmt all green.

## 2026-07-27 — workflow dependency maintenance

Dependabot PRs #71 and #72 update the full-SHA workflow pins for
`actions/checkout` from 7.0.0 to 7.0.1 and OSSF Scorecard from 2.4.3 to 2.4.4.
The checkout patch hardens ref normalization and command argument handling;
Scorecard now carries 5.5.0 and no longer fails the whole action when result
publication fails. Client source, Cargo manifests and lockfile, the published
crate, public types, and protocol behavior are unchanged. Rust stable and MSRV
1.88 tests, cargo audit, CodeQL, and the remaining repository checks passed on
the reviewed heads.

## Summary

| Category                      | Pass  | Issues |
|-------------------------------|-------|--------|
| Endpoints & HTTP Methods      | 9/9   | 0      |
| Request Schemas               | 6/6   | 0      |
| Response Schemas              | 10/10 | 0      |
| Nested Object Schemas         | 7/7   | 0      |
| Enum Values                   | 5/5   | 0      |
| Auth Headers                  | 1/1   | 0      |
| Idempotency                   | 1/1   | 0      |
| Subject Validation            | 1/1   | 0      |
| Response Headers              | 4/4   | 0      |
| Constraint Validation         | 4/4   | 0      |
| Lifecycle Orchestration       | 1/1   | 0      |
| Forward Compatibility         | 1/1   | 0      |

**Client is protocol-conformant.** All endpoints, schemas, enums, headers, and validation constraints match the OpenAPI spec (baseline conformance audit against v0.1.24; additive revisions through v0.1.25.13 / runcycles/cycles-protocol#125 tracked in the dated entries above). Verified against a live server (Java 21 + Redis 7).

---

## Audit Scope

Compared the Rust client implementation against `cycles-protocol-v0.yaml`:
- All 9 endpoints (paths, HTTP methods, request/response schemas)
- All 6 request types and 10 response types (field names, types, required/optional)
- All 7 nested object schemas (Subject, Action, Amount, Caps, Metrics, Balance, ErrorResponse)
- All 5 enum types with exact values
- Auth header (`X-Cycles-API-Key`) and idempotency header (`X-Idempotency-Key`)
- Subject validation (anyOf constraint: at least one standard field)
- Response header capture (`X-Request-Id`, `X-RateLimit-Remaining`, `X-RateLimit-Reset`, `X-Cycles-Tenant`)
- Client-side constraint validation (TTL, grace period, extend_by, non-negative amounts)
- Lifecycle orchestration (reserve → heartbeat → commit/release with RAII guard)
- Forward compatibility (unknown enum values, unknown response fields)

---

## PASS — Correctly Implemented

### Endpoints

| Spec Endpoint                              | Client Method                     | HTTP   | Status |
|--------------------------------------------|-----------------------------------|--------|--------|
| `POST /v1/reservations`                    | `create_reservation()`            | POST   | PASS   |
| `POST /v1/reservations/{id}/commit`        | `commit_reservation()`            | POST   | PASS   |
| `POST /v1/reservations/{id}/release`       | `release_reservation()`           | POST   | PASS   |
| `POST /v1/reservations/{id}/extend`        | `extend_reservation()`            | POST   | PASS   |
| `POST /v1/decide`                          | `decide()`                        | POST   | PASS   |
| `POST /v1/events`                          | `create_event()`                  | POST   | PASS   |
| `GET  /v1/reservations`                    | `list_reservations()`             | GET    | PASS   |
| `GET  /v1/reservations/{id}`               | `get_reservation()`               | GET    | PASS   |
| `GET  /v1/balances`                        | `get_balances()`                  | GET    | PASS   |

All 9 endpoints implemented. Paths match spec exactly. High-level `reserve()` method wraps `create_reservation()` with guard lifecycle.

### Request Schemas

| Spec Schema                  | Rust Type                      | Required Fields                                    | Status |
|------------------------------|--------------------------------|----------------------------------------------------|--------|
| `ReservationCreateRequest`   | `ReservationCreateRequest`     | `idempotency_key`, `subject`, `action`, `estimate` | PASS   |
| `CommitRequest`              | `CommitRequest`                | `idempotency_key`, `actual`                        | PASS   |
| `ReleaseRequest`             | `ReleaseRequest`               | `idempotency_key`                                  | PASS   |
| `ReservationExtendRequest`   | `ExtendRequest`                | `idempotency_key`, `extend_by_ms`                  | PASS   |
| `DecisionRequest`            | `DecisionRequest`              | `idempotency_key`, `subject`, `action`, `estimate` | PASS   |
| `EventCreateRequest`         | `EventCreateRequest`           | `idempotency_key`, `subject`, `action`, `actual`   | PASS   |

All request JSON keys are `snake_case` matching the spec wire format. Rust's native `snake_case` convention means serde serializes directly — no manual mapper code needed (unlike the TypeScript client's 380-line `mappers.ts`).

Optional fields use `#[serde(skip_serializing_if = "Option::is_none")]` to omit `null` values. `dry_run` uses `skip_serializing_if = "is_false"` to omit `false`.

`idempotency_key` is auto-generated (UUID v4) via `bon::Builder` defaults or `::new()` constructors. Always sent in both the request body and the `X-Idempotency-Key` header.

### Response Schemas

| Spec Schema                   | Rust Type                      | JSON Keys Verified | Status |
|-------------------------------|--------------------------------|--------------------|--------|
| `ReservationCreateResponse`   | `ReservationCreateResponse`    | Yes                | PASS   |
| `CommitResponse`              | `CommitResponse`               | Yes                | PASS   |
| `ReleaseResponse`             | `ReleaseResponse`              | Yes                | PASS   |
| `ReservationExtendResponse`   | `ExtendResponse`               | Yes                | PASS   |
| `DecisionResponse`            | `DecisionResponse`             | Yes                | PASS   |
| `EventCreateResponse`         | `EventCreateResponse`          | Yes                | PASS   |
| `ReservationDetail`           | `ReservationDetail`            | Yes                | PASS   |
| `ReservationSummary`          | `ReservationSummary`           | Yes                | PASS   |
| `ReservationListResponse`     | `ReservationListResponse`      | Yes                | PASS   |
| `BalanceResponse`             | `BalanceResponse`              | Yes                | PASS   |

All response structs use `#[non_exhaustive]` — new fields from future server versions are silently ignored during deserialization. Required fields are non-optional; optional fields use `#[serde(default)]`.

### Nested Object Schemas

| Spec Schema       | Rust Type       | Fields                                                     | Status |
|-------------------|-----------------|------------------------------------------------------------|--------|
| `Subject`         | `Subject`       | `tenant`, `workspace`, `app`, `workflow`, `agent`, `toolset`, `dimensions` | PASS |
| `Action`          | `Action`        | `kind`, `name`, `tags`                                     | PASS   |
| `Amount`          | `Amount`        | `unit`, `amount`                                           | PASS   |
| `SignedAmount`    | `SignedAmount`  | `unit`, `amount`                                           | PASS   |
| `Caps`            | `Caps`          | `max_tokens`, `max_steps_remaining`, `tool_allowlist`, `tool_denylist`, `cooldown_ms` | PASS |
| `StandardMetrics` | `CyclesMetrics` | `tokens_input`, `tokens_output`, `latency_ms`, `model_version`, `custom` | PASS |
| `Balance`         | `Balance`       | `scope`, `scope_path`, `remaining`, `reserved`, `spent`, `allocated`, `debt`, `overdraft_limit`, `is_over_limit` | PASS |
| `ErrorResponse`   | `ErrorResponse` | `error`, `message`, `request_id`, `details`                | PASS   |

### Enum Values

| Spec Enum              | Rust Type              | Values                                                                                          | Status |
|------------------------|------------------------|-------------------------------------------------------------------------------------------------|--------|
| `DecisionEnum`         | `Decision`             | `ALLOW`, `ALLOW_WITH_CAPS`, `DENY` + `Unknown` fallback                                        | PASS   |
| `UnitEnum`             | `Unit`                 | `USD_MICROCENTS`, `TOKENS`, `CREDITS`, `RISK_POINTS` + `Unknown` fallback                       | PASS   |
| `CommitOveragePolicy`  | `CommitOveragePolicy`  | `REJECT`, `ALLOW_IF_AVAILABLE`, `ALLOW_WITH_OVERDRAFT`                                          | PASS   |
| `ReservationStatus`    | `ReservationStatus`    | `ACTIVE`, `COMMITTED`, `RELEASED`, `EXPIRED` + `Unknown` fallback                               | PASS   |
| `ErrorCode`            | `ErrorCode`            | All 17 spec values (through v0.1.25.13) + `Unknown` fallback                                     | PASS   |

All enums use `#[serde(rename_all = "SCREAMING_SNAKE_CASE")]` for wire format and `#[non_exhaustive]` + `#[serde(other)]` on an `Unknown` variant for forward compatibility. Unknown server values deserialize successfully instead of failing.

ErrorCode values match spec exactly: `INVALID_REQUEST`, `UNAUTHORIZED`, `FORBIDDEN`, `NOT_FOUND`, `BUDGET_EXCEEDED`, `BUDGET_FROZEN`, `BUDGET_CLOSED`, `RESERVATION_EXPIRED`, `RESERVATION_FINALIZED`, `IDEMPOTENCY_MISMATCH`, `UNIT_MISMATCH`, `OVERDRAFT_LIMIT_EXCEEDED`, `DEBT_OUTSTANDING`, `MAX_EXTENSIONS_EXCEEDED`, `LIMIT_EXCEEDED` (added in spec v0.1.25.12, revision 2026-07-04), `TENANT_CLOSED` (added in spec v0.1.25.13, runcycles/cycles-protocol#125), `INTERNAL_ERROR`.

### Auth & Idempotency

- **`X-Cycles-API-Key`**: Set on every request (POST and GET). Configured via `CyclesClientBuilder::new(api_key, base_url)`. Location: `src/client.rs:297-300` (POST), `src/client.rs:341` (GET).
- **`X-Idempotency-Key`**: Extracted from request body's `idempotency_key` field and sent as header on all POST requests. Location: `src/client.rs:302-305`. Matches spec: "If both header and body idempotency_key are provided, they MUST match."

### Subject Validation

Spec requires `anyOf: [{required: [tenant]}, {required: [workspace]}, ...]` — at least one standard field must be set. Implemented in `src/validation.rs:8-14` via `Subject::has_field()`. Validated before sending in `reserve()` (`src/client.rs:131`). Returns `Error::Validation` if violated.

### Response Header Capture

| Header                  | Captured In               | Location             | Status |
|-------------------------|---------------------------|----------------------|--------|
| `X-Request-Id`          | `ApiResponse.request_id`  | `src/response.rs:41` | PASS   |
| `X-RateLimit-Remaining` | `ApiResponse.rate_limit_remaining` | `src/response.rs:42` | PASS |
| `X-RateLimit-Reset`     | `ApiResponse.rate_limit_reset`     | `src/response.rs:43` | PASS |
| `X-Cycles-Tenant`       | `ApiResponse.cycles_tenant`        | `src/response.rs:44` | PASS |

Available via `_with_metadata()` variants of client methods (e.g., `create_reservation_with_metadata()`).

### Client-Side Constraint Validation

| Constraint              | Spec Bounds              | Validated In              | Status |
|-------------------------|--------------------------|---------------------------|--------|
| `ttl_ms`                | 1000–86400000            | `src/validation.rs:18-24` | PASS   |
| `grace_period_ms`       | 0–60000                  | `src/validation.rs:27-35` | PASS   |
| `extend_by_ms`          | 1–86400000               | `src/validation.rs:38-45` | PASS   |
| `estimate.amount`       | >= 0 (non-negative)      | `src/validation.rs:48-55` | PASS   |

### Lifecycle Orchestration

The `ReservationGuard` RAII type (`src/guard.rs`) implements the reserve → execute → commit/release lifecycle:

1. **Reserve**: `CyclesClient::reserve()` validates input, calls `POST /v1/reservations`, returns `ReservationGuard` on ALLOW/ALLOW_WITH_CAPS, returns `Error::BudgetExceeded` on DENY.
2. **Heartbeat**: Background `tokio::spawn` task extends TTL at `ttl_ms / 2` intervals via `POST /v1/reservations/{id}/extend`. Uses `CancellationToken` for clean shutdown.
3. **Commit**: `guard.commit(self)` consumes the guard (compile-time double-commit prevention) and calls `POST /v1/reservations/{id}/commit`. On a retryable failure (transport error, 5xx, or a transient error code per `Error::is_retryable`) with `retry_enabled`, the commit is retried inline with exponential backoff (`CommitRetryEngine`, `src/retry.rs`), reusing the original request and idempotency key; the heartbeat keeps extending the TTL until the outcome is final, and the returned `Ok`/`Err` is definitive — no background commit activity survives the call.
4. **Release**: `guard.release(self)` consumes the guard, cancels heartbeat, calls `POST /v1/reservations/{id}/release`.
5. **Drop safety**: If guard is dropped without commit/release, `Drop` impl cancels heartbeat and spawns best-effort release via `tokio::runtime::Handle::try_current()`.

### Forward Compatibility

- All response enums use `#[serde(other)]` → Unknown variant for unrecognized values
- All response structs use `#[non_exhaustive]` → new server fields silently ignored
- Tests verify: `"ALLOW_WITH_WARNINGS"` deserializes as `Decision::Unknown`, `"RATE_LIMITED"` as `ErrorCode::Unknown`, `"PENDING"` as `ReservationStatus::Unknown`

---

## Issues Found & Resolved (0.2.3)

1. **Misleading 404 on unit mismatch (issue [#8](https://github.com/runcycles/cycles-client-rust/issues/8))** — The spec defines `Balance` as *"Ledger state for a single **(scope, unit)** balance"* (`cycles-protocol-v0.yaml` line 667), so a single scope may hold multiple budgets keyed by unit. The reference server's `reserve.lua` implements this by keying budgets as `"budget:" .. scope .. ":" .. estimate_unit`. When a reservation targets a scope that has an active budget in a different unit (e.g. stored in `USD_MICROCENTS`, reserved in `TOKENS`), `reserve.lua` finds no matching key, returns `BUDGET_NOT_FOUND`, and the Java layer maps that to `HTTP 404 NOT_FOUND "Budget not found for provided scope: <scope>"`. The raw message reads like a scope-lookup miss, which led users to believe the scope didn't exist.

   Note this surfaces two underlying spec issues on the **server** side (out of scope for the client, to be filed separately against `cycles-server`):
   - The spec for `POST /v1/reservations` (lines 1187–1200) documents responses `200, 400, 401, 403, 409, 500` only — **no 404 is documented**, yet the server returns one here.
   - The spec requires "Unit mismatch on commit ... or event (actual.unit not supported for the target scope) MUST return HTTP 400 with error=UNIT_MISMATCH" (line 56). The analogous rule for *reserve* is under-specified, and the server uses 404 `NOT_FOUND` instead of 400 `UNIT_MISMATCH`.

   The **client** handles the server's out-of-spec response defensively and adds diagnostic context. **Fix:** `create_reservation`, `create_reservation_with_metadata`, `decide`, and `create_event` now post-process errors through `enrich_budget_not_found`, which detects the exact 404 marker and rewrites the `Error::Api.message` field to include the unit that was sent plus a one-line explanation of the `(scope, unit)` indexing invariant. All other `Error::Api` fields (`status`, `code`, `request_id`, `retry_after`, `details`) are preserved unchanged, so error classification, retry logic, request-id correlation, and downstream pattern matching behave identically. `Amount`, `WithCyclesConfig::new`, the `with_cycles_usage` example, and README Quick Start were updated to document the `(scope, unit)` invariant with reference to spec line 667.

## Issues Found & Resolved (0.2.2)

1. **`BlockingCyclesClient::builder()` returned async builder** — `BlockingCyclesClient::builder()` returned `CyclesClientBuilder` whose `build()` produces `CyclesClient` (async), silently giving the wrong client type. **Fix:** removed `BlockingCyclesClient::builder()`; added `CyclesClientBuilder::build_blocking()` (feature-gated behind `blocking`) that returns `Result<BlockingCyclesClient, Error>`.

2. **Missing `Amount::risk_points()` constructor** — `RISK_POINTS` is a first-class unit in the protocol but lacked the convenience constructor that `usd_microcents()`, `tokens()`, and `credits()` all had. **Fix:** added `Amount::risk_points(amount: i64)`.

3. **`SignedAmount` missing all convenience constructors** — `Amount` had four constructors but `SignedAmount` had none, forcing manual struct construction. **Fix:** added `usd_microcents()`, `tokens()`, `credits()`, `risk_points()` to `SignedAmount`.

4. **`BlockingCyclesClient` missing `config()` and `_with_metadata` variant** — async client exposed `config()` and `create_reservation_with_metadata()` but blocking client did not. **Fix:** added both methods.

### Prior Audit (0.2.0–0.2.1)

None. All endpoints, schemas, enums, headers, and validation constraints matched the OpenAPI spec as of the v0.1.24 baseline audit (kept current through the additive revisions tracked in the dated entries at the top of this file).

---

## Test Coverage

Measured with `cargo tarpaulin --skip-clean --out Stdout --ignore-tests -- --skip live` on 2026-07-17.

| Module               | Covered / Total | Coverage |
|----------------------|-----------------|----------|
| `config.rs`          | 81 / 81         | 100%     |
| `error.rs`           | 33 / 33         | 100%     |
| `lifecycle.rs`       | 47 / 47         | 100%     |
| `models/common.rs`   | 26 / 26         | 100%     |
| `models/enums.rs`    | 7 / 7           | 100%     |
| `models/ids.rs`      | 14 / 14         | 100%     |
| `models/request.rs`  | 13 / 13         | 100%     |
| `response.rs`        | 24 / 24         | 100%     |
| `validation.rs`      | 26 / 26         | 100%     |
| `guard.rs`           | 51 / 52         | 98.08%   |
| `client.rs`          | 137 / 143       | 95.80%   |
| `retry.rs`           | 31 / 39         | 79.49%   |
| `heartbeat.rs`       | 6 / 11          | 54.55%   |
| **Overall**          | **496 / 516**   | **96.12%** |

Uncovered lines are concentrated in `heartbeat.rs` background-task wiring and, in `retry.rs`, the interiors of `tracing::debug!`/`warn!` macros (disabled at runtime without a subscriber); all logic branches of the retry loop (success, non-retryable stop, exhaustion, disabled) are exercised.

157 total tests (137 running + 12 live-server ignored + 8 doc-tests): 43 lib unit + 38 wiremock client integration + 18 wire format compliance + 13 error + 10 config + 5 lifecycle + 4 guard lifecycle + 2 response + 4 retry end-to-end. The 0.2.3 release added 5 unit tests (`client::tests::enrich_budget_not_found_*`) and 4 wiremock integration tests (`create_reservation_404_*`, `decide_404_*`, `create_event_404_*`) for the issue #8 fix. The 2026-07-17 commit-retry wiring fix replaced the single non-exercising retry test with 4 end-to-end guard-path retry tests and moved the shared reserve-mock scaffold to `tests/common/mod.rs`.

---

## Verdict

The Rust client (`runcycles` crate) is fully conformant with the Cycles Budget Authority API (baseline audit v0.1.24; kept current through the additive spec revisions — most recently v0.1.25.13, runcycles/cycles-protocol#125 — tracked in the dated entries at the top of this file). All 9 endpoints, 6 request schemas, 10 response schemas, 7 nested object types, and 5 enum types match the OpenAPI specification exactly. Wire format serialization uses serde's native snake_case, eliminating the manual mapper layer needed in other clients. Forward compatibility is ensured via `#[non_exhaustive]` structs and `#[serde(other)]` enum fallbacks. The RAII guard pattern provides compile-time lifecycle safety not achievable in other client languages. No protocol violations found.
