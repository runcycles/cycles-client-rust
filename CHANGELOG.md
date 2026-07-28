# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/).

## [0.3.1] - 2026-07-27

Heartbeat extend-drift fix (P1 liveness, fleet-wide — same bug in all four SDKs), refined under five rounds of adversarial + spec review: alternate-beat → lead-estimate → grant-ledger → grant-ledger with immediate first beat and lead-clamp regime (v2.3) → **server-authoritative `remaining_ttl_ms` scheduling with the v2.3 heuristic as fallback (round 5)**.

### Added

- **`remaining_ttl_ms` support (spec PR #148) — normative heartbeat scheduling.** Round 5 of the spec review proved regime detection from `(grant, elapsed)` samples **undecidable in general**: any real per-extend grant in the sticky window `[0.75·min(ttl/2, 30 s), 0.9·ttl)` tracks the held cadence closely enough to stay classified lead-clamp while the lease erodes to a lapse (e.g. ttl 24 s with real +10 s grants → held cadence 12 s → post-skip ratio 10/12 sits inside the `[0.75, 1.25]` band forever, losing 2 s per cycle). The protocol therefore gained a server-authoritative field: `remaining_ttl_ms` (int64 ≥ 0) on **both** `ReservationCreateResponse` and `ExtendResponse` — the remaining reservation lifetime in ms at response evaluation, same clock snapshot as `expires_at_ms`, present on successful live-reservation responses (absent on dry-run/DENY and on older servers; the field is optional in the client models for back-compat).

  When a successful response carries the field, scheduling is **normative**: `lead_floor = max(0, remaining_ttl_ms − rtt)` (rtt = monotonic elapsed between sending the call and receiving the response; unknown → 0), `retry_reserve = min(lead_floor/2, max(1 s, 2·max_observed_rtt))`, and the next beat lands `lead_floor − retry_reserve` after response receipt — recomputed from **every** field-carrying response, never from accumulated expiry differences. The `lead_min` skip heuristic is **bypassed** in this mode (the schedule is exact; a heuristic skip could overshoot the real lease), but the grant ledger keeps running in the background so the heuristic resumes seamlessly if a later response omits the field (mixed fleets, rollbacks). A transient failure retries with the **same idempotency key** after `clamp(lead_estimate/4, 1 s, 30 s)`, where `lead_estimate` is the last `lead_floor` minus the monotonic time since that response (saturating at 0). When the **create** response carries the field, the first beat is derived from the same formula instead of the immediate prime — a 60 s remaining lease first beats at 59 s, a tenant-capped 1 s lease at 500 ms (inside the lease), and no primed extension is spent even under a maximum-lead clamp. Extend amount (requested ttl), permanent stops, and 2xx-as-applied are unchanged.

### Fixed

- **Heartbeat no longer drifts the reservation expiry outward — and no longer lapses it either.** The protocol's `extend_by_ms` extends **relative to the reservation's current `expires_at_ms`**, not to request time (`cycles-protocol-v0.yaml`), but the heartbeat sent `ExtendRequest::new(ttl_ms)` on *every* `ttl/2` beat — drifting the expiry outward by `ttl/2` per beat. Consequences: a killed process left the reserved budget locked until the drifted expiry (a zombie window that grew with runtime, capped only by the server's `max_extensions` ≈ 10 → up to ~6×ttl at defaults), and extensions burned twice as fast as needed, so runs longer than ~`max_extensions × ttl/2` exhausted the allowance and lost heartbeat protection mid-flight.

  The fix is a **grant-ledger** scheduler (`src/heartbeat.rs`). Correctness rests on a rigorous, conservative **lower bound** of the expiry lead: `lead_min = grants_sum − elapsed` (signed, starts at 0), where each grant is the difference of two *successive server-frame `expires_at_ms` values* — the previous known expiry (seeded from the reserve response, now threaded from the guard into the heartbeat) vs. the extend response's — and `elapsed` is client-monotonic ms. No cross-clock arithmetic anywhere: server frame minus server frame, monotonic minus monotonic, so clock skew cannot corrupt the bound, and it under-estimates by construction (the initial grant is deliberately uncounted — its true size is unknowable without cross-clock math — and the monotonic anchor starts *after* the server stamped the initial expiry). A beat skips only when a grant sample exists and `lead_min ≥ 1.5·last_grant`; otherwise it extends by the **requested** `ttl_ms` (the server's clamp shows up in the ledger, never in the wire amount). Beat delays are computed per beat rather than from a fixed interval: the **first beat is immediate** (see the round-4 follow-up below), then `clamp(grant/2, 500 ms, requested/2)` — the cadence tracks what the server actually grants (a clamped grant *speeds up* the beats) — unless the grant is classified as **lead-clamped**, in which case the cadence is *held* at `min(requested/2, 30 s)` (also the retry pace before any grant sample exists, so a failed immediate beat can never hot-loop). A transient failure retries at the current cadence. Each next beat is scheduled from the previous beat's intended instant (no per-beat RTT slip), realigned to "now" after a stall instead of bursting — the `MissedTickBehavior::Skip` equivalent, hand-rolled because delays now vary per beat. Net: no drift (skips whenever the bound proves ample lead), no lapse (any shortfall — failures, clamped grants, small TTLs — extends on the very next beat), and steady-state extension consumption is still roughly halved.

  The first cut of this fix shipped a fixed **alternate-beat** cadence (extend, then skip-after-success). Adversarial self-review found confirmed liveness regressions in it, all corrected by the redesign:
  - **Single-failure lapse at zero margin:** a failed extend left exactly one retry beat before expiry; the lead estimate instead extends whenever the margin is short, regardless of cadence history.
  - **Guaranteed lapse for spec-legal `ttl_ms` in (1000, 2000):** the interval had a 1-second floor, so e.g. ttl 1200 got its first beat at 1000 ms and then a skip cycle past expiry. The floor is **removed** — the interval is exactly `ttl/2` (spec minimum ttl 1000 → 500 ms beats).
  - **Beat slip by one RTT per cycle:** sleeping after each awaited extend pushed every beat later by the request's round trip. Beats are now anchored to intended instants (the second cut used `tokio::time::interval_at` with `MissedTickBehavior::Skip`; v2.2+ keeps the same no-slip / no-burst semantics with per-beat computed sleeps, since delays now vary); the lead math uses the beat's intended instant.
  - **Every-beat re-extend on unrecognized 2xx statuses (drift):** an HTTP-success extend whose status wasn't `ACTIVE` was treated as failure and retried each beat — but a 2xx means the server **did** apply the extension, and its `expires_at_ms` is authoritative proof. **Reversed:** any 2xx counts as applied; `known_expiry` is updated from `expires_at_ms` and the odd status is only warned about.
  - **Permanent failures retried forever:** `RESERVATION_EXPIRED`, `RESERVATION_FINALIZED`, `MAX_EXTENSIONS_EXCEEDED`, or any HTTP 410 now **stop the heartbeat** (logged once) — no extend can ever succeed again, so retrying is pure traffic and log noise.
  - **Fresh idempotency key per retry risked double-extend:** an applied-but-lost extension followed by a fresh-key retry would extend twice. A transient failure now **keeps its idempotency key** and the next beat reuses it (server-side replay dedupes); a fresh key is used only after the previous outcome resolved.

  Spec-review follow-ups (rounds 2–4, same release): **tenant policy `max_reservation_ttl_ms` silently caps the granted TTL** at reserve (governance default 1 hour), and the create response has no effective-TTL field — so seeding the heartbeat from the *requested* TTL alone schedules the first beat far too late (a 24 h request capped to 1 h → first beat at 12 h, 11 h after expiry). Round 2 recovered an "effective TTL" as `clamp(expires_at_ms − Date, 1000 ms, requested)` and let it drive the whole scheduler; **round 3 rejected the HTTP `Date` header as a correctness input**: RFC 9110's `Date` is a whole-second, *best-effort origination* timestamp that intermediaries may replace, and in cycles-server `expires_at_ms` is stamped from Redis `TIME` while `Date` comes from the HTTP layer — not the same clock, so the difference is not a lease measurement, and the 1000 ms upward clamp could *fabricate* lease the server never granted. **Round 4 removed lease estimation from scheduling entirely**, on two confirmed findings:
  - **Any bounded first-beat delay can outlive a small capped lease** (a 30 s cap is still 28 s too late for a 2 s grant), so v2.3's **first extend fires immediately**. It costs one extension, but it is the only schedule that provably beats an arbitrarily small lease — and its response primes the grant ledger with a *real* grant sample that paces every later beat. The `Date`-derived hint is gone from the heartbeat path; `ApiResponse::date_ms` and the `httpdate` parsing (a direct dependency — already in the tree via hyper) remain as general response utilities the SDK derives no behavior from.
  - **Grant-derived cadence is only valid for real per-extend grants.** Under a server-side **maximum-LEAD clamp** (every extend re-stamps `expires_at ≈ now + L` instead of adding lease), successive `expires_at_ms` differences measure *elapsed time*, not lease — so pacing by them is self-referential: the observed "grant" shrinks to whatever the cadence is, the cadence halves in response, and within a few beats it collapses to the 500 ms floor, burning `max_extensions` in seconds. v2.3 classifies each success (`is_lead_clamp_grant`): a grant that is non-positive, or that is both `< 0.9·requested` and within `[0.75, 1.25]×` the elapsed time since the last success (the signature of a clock reading, not a lease), enters the **lead-clamp regime** — cadence held at `min(requested/2, 30 s)`, never tightened, with a `tracing::warn` once per heartbeat (the allowance is still depleting, just at the held pace). The lower `0.75×` band arm lets a *real* but small per-extend grant recover: after a skip doubles the inter-success gap a fixed grant falls below the band and the cadence tightens again, whereas a lead-clamped "grant" tracks the gap and stays inside the band. **Round 5 proved this band undecidable in general** (see the `remaining_ttl_ms` entry above): it survives only as a best-effort fallback for legacy servers that clamp per-extend deltas; `remaining_ttl_ms` is the normative path.

  Correctness lives entirely in the grant ledger above: from the first (immediate) extend response onward the cadence follows the server's *observed* grants, which — unlike any `Date` arithmetic — are same-frame by construction. The permanent stop set also gained `TENANT_CLOSED` (tenant closure is irreversible without administrative action) and `NOT_FOUND` / raw HTTP 404 (a 404'd reservation never returns).

  Cancellation semantics are unchanged (`CancellationToken`; the guard cancels on commit/release/drop). Regression tests (`tests/heartbeat_test.rs`, wiremock with dynamic `expires_at_ms` responders) pin: the immediate first beat (an extend arrives well before ttl/2) and the v2.3 full-grant cadence extend@0/1000/2000 ms / skip@3000 (bound exactly `1.5·grant`, inclusive) / extend@4000; the capped scenario (requested 8000 / granted 2000 → the immediate beat discovers the cap that a requested/2 schedule would have found 2 s after expiry, cadence tracking the *observed* grant at 1000 ms, wire `extend_by_ms` staying the requested 8000); a 503 on the immediate first beat retrying at the held cadence — exactly one attempt inside the first margin, never a zero-delay hot loop — with the **same** idempotency key, then a fresh key after success (asserted from received request bodies); permanent-failure stop for 409 `MAX_EXTENSIONS_EXCEEDED`, 409 `TENANT_CLOSED`, and 404 `NOT_FOUND` (no further requests); ttl 1200 staying alive across 600 ms beats with the skip landing exactly at the threshold beat; a per-extend grant clamp (+ttl/4) still *tightening* the cadence to grant/2 = 500 ms; a **lead-clamp responder** (echoing `reserve_expiry + elapsed-at-receipt`) holding the cadence at requested/2 instead of collapsing to the 500 ms floor; a zero-grant immediate prime holding the cadence likewise; and a 200 with an unknown status counting as applied (fresh key next beat + the steady-state skip still occurring — pinning both the resolution and the ledger update). The pure cadence/regime computations are extracted as functions and unit-tested in `src/heartbeat.rs` — including the 30 s held-cadence cap for huge TTLs, which would be impractical to wait out in wall-clock tests — alongside skip-threshold, grant-cadence, lead-clamp-band boundary, and permanent-classification tests; `Date`-parsing unit tests remain in `src/response.rs` (the parsing is now a general utility, exercised end-to-end in `tests/response_test.rs`).

  **Normative-mode tests** (round 5): wiremock responders now optionally emit `remaining_ttl_ms` (constant, or only on the first *n* responses) and pin — no immediate prime and a `lead_floor − retry_reserve` first beat when the create carries the field; ~1 s steady normative cadence with the `lead_min` skip **bypassed** even when accumulated grants would trip it; a tenant-capped 1 s lease first-beating at ~500 ms (inside the lease); a max-lead-clamping field-carrying server scheduling at ~cap − reserve with no cadence collapse and no primed extension; the field disappearing mid-flight with the heuristic resuming at the observed cadence **and** skipping on the ledger maintained through the normative phase; and a transient failure in normative mode retrying with the same idempotency key after `clamp(lead/4, 1 s, 30 s)`. Pure-function pins in `src/heartbeat.rs` cover the 59 s delay for a 60 s remaining lease, rtt/max-rtt widening, the lead-floor saturation, and the retry clamp bounds; wire-format serde tests in `tests/models_test.rs` cover the optional field on both responses.

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

- **`Retry-After` is parsed and honored.** Error responses now populate `Error::retry_after()` from the `Retry-After` header (delta-seconds form; previously always `None`), and the retry loop waits **at least** the server's advertised delay after an HTTP 429 `LIMIT_EXCEEDED` — even when that exceeds `retry_max_delay`. Each response's `Retry-After` is consumed exactly once (it governs only the sleep immediately following it); plain exponential backoff applies otherwise. The honored `Retry-After` is clamped to **1 hour** (fleet decision D2) so a bogus or hostile header cannot park a retry loop indefinitely.

### Hardening (adversarial self-review)

- **Bodyless 429 is retryable.** `Error::is_retryable()` now treats HTTP 429 as retryable **by status alone** — a 429 whose body is absent or unparseable (no typed `LIMIT_EXCEEDED` code, e.g. proxy/LB interposition) is still retried, honoring the `Retry-After` header. Cross-SDK parity.
- **HTTP 410 triggers the event fallback.** Recovery fires on `RESERVATION_EXPIRED` **or** any HTTP 410 response — a mangled/non-JSON 410 body still recovers the spend. New `Error::status()` accessor exposes the originating HTTP status.
- **Heartbeat stops before recovery.** The heartbeat task is cancelled *before* the event fallback runs (the reservation is expired for good; further extends are doomed traffic and log noise).
- **Non-`APPLIED` event status is not recovery.** An HTTP-success fallback event whose `status` is anything but `APPLIED` (including forward-compat `Unknown`) is treated as recovery failure → `Error::CommitRecoveryFailed`, with the unexpected status conveyed in `event_error`.
- **`RECOVERED_VIA_EVENT` wire-guarded.** The wire string `"RECOVERED_VIA_EVENT"` deserializes to `CommitStatus::Unknown` (it is client-synthesized only, documented never-server-sent), so a non-conformant server cannot fabricate a recovery and violate the `recovered_via_event` `Some`-iff-status invariant.
- **`BudgetExceeded` retryability tightened.** Retryable only when the error came from an actual HTTP 429 **and** carries a retry delay; a 409 `BUDGET_EXCEEDED` with a `Retry-After` header (or a `DENY`-decision denial with `retry_after_ms`) is a budget fact and is no longer reported retryable. `Error::BudgetExceeded` gained a `status: Option<u16>` field (`None` for `DENY`-derived).
- **Cancellation contract documented.** `commit()`'s docs state that recovery is not cancellation-transparent: if the future is dropped mid-recovery the outcome is unknown, and the caller may safely re-`POST /v1/events` with the commit's idempotency key (exactly-once server-side).

### Notes

- **Source-level breakage:** `Error` is not `#[non_exhaustive]`, so downstream `match`es over all `Error` variants must add a `CommitRecoveryFailed` arm, and exhaustive constructors/patterns of `Error::BudgetExceeded` must account for the new `status` field — hence the 0.3.0 minor bump. Everything else is additive (`CommitStatus` and `CommitResponse` are `#[non_exhaustive]`; `ReservationGuard::new` is `pub(crate)`).
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
