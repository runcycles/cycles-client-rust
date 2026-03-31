//! Basic low-level client usage.
//!
//! Demonstrates creating a reservation, committing, and error handling
//! using the low-level client API directly.

use runcycles::models::*;
use runcycles::CyclesClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = CyclesClient::builder("my-api-key", "http://localhost:7878")
        .tenant("acme")
        .build();

    // Create a reservation
    let resp = client
        .create_reservation(
            &ReservationCreateRequest::builder()
                .subject(Subject {
                    tenant: Some("acme".into()),
                    ..Default::default()
                })
                .action(Action::new("llm.completion", "gpt-4o"))
                .estimate(Amount::usd_microcents(5000))
                .build(),
        )
        .await?;

    println!("Decision: {:?}", resp.decision);

    if resp.decision.is_allowed() {
        let id = resp.reservation_id.expect("id present when allowed");
        println!("Reservation ID: {id}");

        // Simulate work...

        // Commit actual spend
        let commit_resp = client
            .commit_reservation(
                &id,
                &CommitRequest::builder()
                    .actual(Amount::usd_microcents(3200))
                    .build(),
            )
            .await?;

        println!("Committed: {:?}", commit_resp.charged);
    }

    Ok(())
}
