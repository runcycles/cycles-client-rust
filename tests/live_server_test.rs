//! Live integration tests against a real Cycles server.
//!
//! These tests read configuration from environment variables:
//! - `CYCLES_API_KEY`  — defaults to "cyc_live_newky234567890abcdef"
//! - `CYCLES_BASE_URL` — defaults to "http://localhost:7878"
//! - `CYCLES_TENANT`   — defaults to "demo-corp"
//!
//! Run with: cargo test --test live_server_test -- --ignored
//! These tests are `#[ignore]` by default — they only run when explicitly requested.

use runcycles::models::*;
use runcycles::{CyclesClient, Error};

const DEFAULT_API_KEY: &str = "cyc_live_newky234567890abcdef";
const DEFAULT_BASE_URL: &str = "http://localhost:7878";
const DEFAULT_TENANT: &str = "demo-corp";

fn api_key() -> String {
    std::env::var("CYCLES_API_KEY").unwrap_or_else(|_| DEFAULT_API_KEY.to_string())
}

fn base_url() -> String {
    std::env::var("CYCLES_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string())
}

fn tenant() -> String {
    std::env::var("CYCLES_TENANT").unwrap_or_else(|_| DEFAULT_TENANT.to_string())
}

fn client() -> CyclesClient {
    CyclesClient::builder(&api_key(), &base_url())
        .tenant(&tenant())
        .build()
}

fn subject() -> Subject {
    Subject {
        tenant: Some(tenant()),
        ..Default::default()
    }
}

// ─── Full reserve → commit lifecycle ─────────────────────────────

#[tokio::test]
#[ignore]
async fn live_full_lifecycle_reserve_commit() {
    let client = client();

    // 1. Reserve
    let guard = client
        .reserve(
            ReservationCreateRequest::builder()
                .subject(subject())
                .action(Action::new("llm.completion", "gpt-4o"))
                .estimate(Amount::tokens(500))
                .ttl_ms(30_000_u64)
                .build(),
        )
        .await
        .expect("reserve should succeed");

    assert_eq!(guard.decision(), Decision::Allow);
    assert!(!guard.reservation_id().as_str().is_empty());
    println!("Reserved: {}", guard.reservation_id());

    // 2. Commit with actual < estimate (delta returned)
    let commit_resp = guard
        .commit(
            CommitRequest::builder()
                .actual(Amount::tokens(300))
                .metrics(CyclesMetrics {
                    tokens_input: Some(100),
                    tokens_output: Some(200),
                    latency_ms: Some(1500),
                    model_version: Some("gpt-4o-2024-05".to_string()),
                    ..Default::default()
                })
                .build(),
        )
        .await
        .expect("commit should succeed");

    assert_eq!(commit_resp.status, CommitStatus::Committed);
    assert_eq!(commit_resp.charged.amount, 300);
    assert_eq!(commit_resp.charged.unit, Unit::Tokens);
    // Delta released: 500 - 300 = 200
    if let Some(released) = &commit_resp.released {
        assert_eq!(released.amount, 200);
    }
    println!("Committed: {} tokens", commit_resp.charged.amount);
}

// ─── Full reserve → release lifecycle ────────────────────────────

#[tokio::test]
#[ignore]
async fn live_full_lifecycle_reserve_release() {
    let client = client();

    let guard = client
        .reserve(
            ReservationCreateRequest::builder()
                .subject(subject())
                .action(Action::new("llm.completion", "gpt-4o"))
                .estimate(Amount::tokens(200))
                .build(),
        )
        .await
        .expect("reserve should succeed");

    let rsv_id = guard.reservation_id().clone();
    println!("Reserved: {}", rsv_id);

    // Release the reservation
    let release_resp = guard
        .release("test_cancellation")
        .await
        .expect("release should succeed");

    assert_eq!(release_resp.status, ReleaseStatus::Released);
    assert_eq!(release_resp.released.amount, 200);
    println!("Released: {} tokens", release_resp.released.amount);

    // Verify the reservation is now in RELEASED state
    let detail = client
        .get_reservation(&rsv_id)
        .await
        .expect("get_reservation should succeed");

    assert_eq!(detail.status, ReservationStatus::Released);
    println!("Status confirmed: {:?}", detail.status);
}

// ─── Low-level API: create → commit ─────────────────────────────

#[tokio::test]
#[ignore]
async fn live_low_level_create_commit() {
    let client = client();

    // Create reservation via low-level API
    let create_resp = client
        .create_reservation(
            &ReservationCreateRequest::builder()
                .subject(subject())
                .action(Action::new("tool.search", "web_search"))
                .estimate(Amount::tokens(100))
                .build(),
        )
        .await
        .expect("create_reservation should succeed");

    assert!(create_resp.decision.is_allowed());
    let rsv_id = create_resp
        .reservation_id
        .expect("should have reservation_id");
    assert!(!create_resp.affected_scopes.is_empty());
    println!("Created: {}", rsv_id);

    // Commit via low-level API
    let commit_resp = client
        .commit_reservation(
            &rsv_id,
            &CommitRequest::builder().actual(Amount::tokens(80)).build(),
        )
        .await
        .expect("commit should succeed");

    assert_eq!(commit_resp.status, CommitStatus::Committed);
    assert_eq!(commit_resp.charged.amount, 80);
    println!("Committed: {} tokens", commit_resp.charged.amount);
}

// ─── Decide (preflight) ─────────────────────────────────────────

#[tokio::test]
#[ignore]
async fn live_decide_preflight() {
    let client = client();

    let resp = client
        .decide(
            &DecisionRequest::builder()
                .subject(subject())
                .action(Action::new("llm.completion", "gpt-4o"))
                .estimate(Amount::tokens(1000))
                .build(),
        )
        .await
        .expect("decide should succeed");

    assert!(resp.decision.is_allowed());
    println!("Decision: {:?}", resp.decision);
}

// ─── Events (direct debit) ──────────────────────────────────────

#[tokio::test]
#[ignore]
async fn live_create_event() {
    let client = client();

    let resp = client
        .create_event(
            &EventCreateRequest::builder()
                .subject(subject())
                .action(Action::new("tool.calculator", "math"))
                .actual(Amount::tokens(50))
                .build(),
        )
        .await
        .expect("create_event should succeed");

    assert_eq!(resp.status, EventStatus::Applied);
    assert!(!resp.event_id.as_str().is_empty());
    println!("Event: {}", resp.event_id);
}

// ─── Balances ────────────────────────────────────────────────────

#[tokio::test]
#[ignore]
async fn live_get_balances() {
    let client = client();

    // First create and commit a reservation to ensure balance data exists
    let guard = client
        .reserve(
            ReservationCreateRequest::builder()
                .subject(subject())
                .action(Action::new("llm.completion", "gpt-4o"))
                .estimate(Amount::tokens(10))
                .build(),
        )
        .await
        .expect("reserve should succeed");

    guard
        .commit(CommitRequest::builder().actual(Amount::tokens(5)).build())
        .await
        .expect("commit should succeed");

    let resp = client
        .get_balances(&BalanceParams {
            tenant: Some(tenant()),
            ..Default::default()
        })
        .await
        .expect("get_balances should succeed");

    // Server returns balances (may be empty if server filters differently)
    println!("Balances returned: {} entries", resp.balances.len());
    for b in &resp.balances {
        println!("  {} remaining: {}", b.scope, b.remaining.amount);
    }
}

// ─── List reservations ──────────────────────────────────────────

#[tokio::test]
#[ignore]
async fn live_list_reservations() {
    let client = client();

    let resp = client
        .list_reservations(&ListReservationsParams::default())
        .await
        .expect("list_reservations should succeed");

    println!("Found {} reservations", resp.reservations.len());
    // Should have at least the ones we created in other tests
    // (but tests run in parallel so we can't guarantee count)
}

// ─── Extend (heartbeat) ─────────────────────────────────────────

#[tokio::test]
#[ignore]
async fn live_reserve_extend_commit() {
    let client = client();

    let guard = client
        .reserve(
            ReservationCreateRequest::builder()
                .subject(subject())
                .action(Action::new("llm.streaming", "gpt-4o"))
                .estimate(Amount::tokens(1000))
                .ttl_ms(10_000_u64) // Short TTL
                .build(),
        )
        .await
        .expect("reserve should succeed");

    let rsv_id = guard.reservation_id().clone();
    println!("Reserved: {} (short TTL)", rsv_id);

    // Manually extend
    guard.extend(30_000).await.expect("extend should succeed");
    println!("Extended TTL by 30s");

    // Commit
    let resp = guard
        .commit(CommitRequest::builder().actual(Amount::tokens(800)).build())
        .await
        .expect("commit should succeed");

    assert_eq!(resp.status, CommitStatus::Committed);
    println!("Committed after extend: {} tokens", resp.charged.amount);
}

// ─── Metadata with response headers ─────────────────────────────

#[tokio::test]
#[ignore]
async fn live_response_metadata() {
    let client = client();

    let resp = client
        .create_reservation_with_metadata(
            &ReservationCreateRequest::builder()
                .subject(subject())
                .action(Action::new("llm.completion", "gpt-4o"))
                .estimate(Amount::tokens(100))
                .build(),
        )
        .await
        .expect("should succeed");

    // Server should return request ID header
    assert!(resp.request_id.is_some(), "should have x-request-id");
    println!("Request ID: {}", resp.request_id.as_deref().unwrap());
    println!("Decision: {:?}", resp.data.decision);

    // Clean up: release the reservation
    if let Some(rsv_id) = &resp.data.reservation_id {
        let _ = client
            .release_reservation(rsv_id, &ReleaseRequest::new(Some("cleanup".into())))
            .await;
    }
}

// ─── Error handling: auth failure ────────────────────────────────

#[tokio::test]
#[ignore]
async fn live_auth_failure() {
    let bad_client = CyclesClient::builder("bad-key", &base_url()).build();

    let err = bad_client
        .create_reservation(
            &ReservationCreateRequest::builder()
                .subject(subject())
                .action(Action::new("llm.completion", "gpt-4o"))
                .estimate(Amount::tokens(100))
                .build(),
        )
        .await
        .expect_err("should fail with bad API key");

    match &err {
        Error::Api { status, code, .. } => {
            assert_eq!(*status, 401);
            assert_eq!(*code, Some(ErrorCode::Unauthorized));
        }
        _ => panic!("expected Api error, got: {:?}", err),
    }
    println!("Auth failure handled correctly: {}", err);
}

// ─── Guard auto-release on drop ──────────────────────────────────

#[tokio::test]
#[ignore]
async fn live_guard_drop_auto_releases() {
    let client = client();

    let rsv_id;
    {
        let guard = client
            .reserve(
                ReservationCreateRequest::builder()
                    .subject(subject())
                    .action(Action::new("llm.completion", "gpt-4o"))
                    .estimate(Amount::tokens(100))
                    .build(),
            )
            .await
            .expect("reserve should succeed");

        rsv_id = guard.reservation_id().clone();
        println!("Reserved: {} (will be dropped)", rsv_id);
        // Guard drops here without commit or release
    }

    // Give the spawned release task time to execute
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Verify the reservation was released
    let detail = client
        .get_reservation(&rsv_id)
        .await
        .expect("get_reservation should succeed");

    assert_eq!(
        detail.status,
        ReservationStatus::Released,
        "guard drop should have auto-released the reservation"
    );
    println!("Auto-release confirmed: {:?}", detail.status);
}

// ─── Dry run ─────────────────────────────────────────────────────

#[tokio::test]
#[ignore]
async fn live_dry_run() {
    let client = client();

    let resp = client
        .create_reservation(
            &ReservationCreateRequest::builder()
                .subject(subject())
                .action(Action::new("llm.completion", "gpt-4o"))
                .estimate(Amount::tokens(500))
                .dry_run(true)
                .build(),
        )
        .await
        .expect("dry_run should succeed");

    assert!(resp.decision.is_allowed());
    // Dry run should NOT create a reservation_id
    assert!(
        resp.reservation_id.is_none(),
        "dry_run should not create a reservation_id"
    );
    println!("Dry run decision: {:?}", resp.decision);
}
