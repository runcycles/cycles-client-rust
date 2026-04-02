# Protocol Conformance Audit — Rust Client

- **Date:** 2026-04-02
- **Spec:** `cycles-protocol-v0.yaml` v0.1.24 (OpenAPI 3.1.0)
- **Client:** Rust 1.88+ (MSRV), reqwest 0.12, serde 1, tokio 1, bon 3
- **Cross-reference:** [cycles-server AUDIT.md](https://github.com/runcycles/cycles-server/blob/main/AUDIT.md)

---

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

**Client is protocol-conformant.** All endpoints, schemas, enums, headers, and validation constraints match the OpenAPI spec v0.1.24. Verified against a live server (Java 21 + Redis 7).

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
| `ErrorCode`            | `ErrorCode`            | All 15 spec values + `Unknown` fallback                                                          | PASS   |

All enums use `#[serde(rename_all = "SCREAMING_SNAKE_CASE")]` for wire format and `#[non_exhaustive]` + `#[serde(other)]` on an `Unknown` variant for forward compatibility. Unknown server values deserialize successfully instead of failing.

ErrorCode values match spec exactly: `INVALID_REQUEST`, `UNAUTHORIZED`, `FORBIDDEN`, `NOT_FOUND`, `BUDGET_EXCEEDED`, `BUDGET_FROZEN`, `BUDGET_CLOSED`, `RESERVATION_EXPIRED`, `RESERVATION_FINALIZED`, `IDEMPOTENCY_MISMATCH`, `UNIT_MISMATCH`, `OVERDRAFT_LIMIT_EXCEEDED`, `DEBT_OUTSTANDING`, `MAX_EXTENSIONS_EXCEEDED`, `INTERNAL_ERROR`.

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
3. **Commit**: `guard.commit(self)` consumes the guard (compile-time double-commit prevention), cancels heartbeat, calls `POST /v1/reservations/{id}/commit`.
4. **Release**: `guard.release(self)` consumes the guard, cancels heartbeat, calls `POST /v1/reservations/{id}/release`.
5. **Drop safety**: If guard is dropped without commit/release, `Drop` impl cancels heartbeat and spawns best-effort release via `tokio::runtime::Handle::try_current()`.

### Forward Compatibility

- All response enums use `#[serde(other)]` → Unknown variant for unrecognized values
- All response structs use `#[non_exhaustive]` → new server fields silently ignored
- Tests verify: `"ALLOW_WITH_WARNINGS"` deserializes as `Decision::Unknown`, `"RATE_LIMITED"` as `ErrorCode::Unknown`, `"PENDING"` as `ReservationStatus::Unknown`

---

## Issues Found & Resolved (0.2.2)

1. **`BlockingCyclesClient::builder()` returned async builder** — `BlockingCyclesClient::builder()` returned `CyclesClientBuilder` whose `build()` produces `CyclesClient` (async), silently giving the wrong client type. **Fix:** removed `BlockingCyclesClient::builder()`; added `CyclesClientBuilder::build_blocking()` (feature-gated behind `blocking`) that returns `Result<BlockingCyclesClient, Error>`.

2. **Missing `Amount::risk_points()` constructor** — `RISK_POINTS` is a first-class unit in the protocol but lacked the convenience constructor that `usd_microcents()`, `tokens()`, and `credits()` all had. **Fix:** added `Amount::risk_points(amount: i64)`.

### Prior Audit (0.2.0–0.2.1)

None. All endpoints, schemas, enums, headers, and validation constraints match the OpenAPI spec v0.1.24.

---

## Test Coverage

| Module               | Coverage |
|----------------------|----------|
| `models/enums.rs`    | 100%     |
| `models/common.rs`   | 100%     |
| `models/ids.rs`      | 100%     |
| `models/request.rs`  | 100%     |
| `validation.rs`      | 100%     |
| `guard.rs`           | 100%     |
| `config.rs`          | 100%     |
| `lifecycle.rs`       | 100%     |
| `client.rs`          | 95%      |
| `response.rs`        | 96%      |
| `retry.rs`           | 80%      |
| `error.rs`           | 91%      |
| `heartbeat.rs`       | 64%      |
| **Overall**          | **95.3%**|

141 total tests: 37 unit + 26 wiremock integration + 18 wire format compliance + 10 error + 10 config + 5 lifecycle + 4 guard lifecycle + 2 response + 1 retry + 12 live server (ignored by default) + 8 doc-tests.

---

## Verdict

The Rust client (`runcycles` crate) is fully conformant with the Cycles Budget Authority API v0.1.24. All 9 endpoints, 6 request schemas, 10 response schemas, 7 nested object types, and 5 enum types match the OpenAPI specification exactly. Wire format serialization uses serde's native snake_case, eliminating the manual mapper layer needed in other clients. Forward compatibility is ensured via `#[non_exhaustive]` structs and `#[serde(other)]` enum fallbacks. The RAII guard pattern provides compile-time lifecycle safety not achievable in other client languages. No protocol violations found.
