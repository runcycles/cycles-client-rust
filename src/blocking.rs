//! Synchronous (blocking) wrapper for the Cycles client.
//!
//! This module is only available when the `blocking` feature is enabled.
//! It wraps the async [`CyclesClient`](crate::CyclesClient) with an internal
//! tokio runtime, following the same pattern as `reqwest::blocking`.

#[cfg(feature = "blocking")]
pub mod sync_client {
    use crate::config::CyclesConfig;
    use crate::error::Error;
    use crate::models::request::*;
    use crate::models::response::*;
    use crate::models::ReservationId;
    use crate::response::ApiResponse;

    /// Synchronous (blocking) client for the Cycles API.
    ///
    /// Wraps the async client with an internal tokio runtime.
    pub struct BlockingCyclesClient {
        inner: crate::CyclesClient,
        rt: tokio::runtime::Runtime,
    }

    impl BlockingCyclesClient {
        /// Create a blocking client from a config.
        pub fn new(config: CyclesConfig) -> Result<Self, Error> {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .map_err(|e| Error::Config(format!("failed to create tokio runtime: {e}")))?;
            // Construct inside the live multi-thread runtime so automatic
            // replay starts without making the blocking constructor wait for
            // a persisted Retry-After floor.
            let inner = rt.block_on(async { crate::CyclesClient::new(config) });
            Ok(Self { inner, rt })
        }

        /// Access the client configuration.
        pub fn config(&self) -> &CyclesConfig {
            self.inner.config()
        }

        /// Replay unresolved settlements from the durable journal.
        pub fn flush_pending_commits(&self) -> usize {
            self.rt.block_on(self.inner.flush_pending_commits())
        }

        /// Replay unresolved settlements, waiting at most `timeout`.
        pub fn flush_pending_commits_with_timeout(&self, timeout: std::time::Duration) -> usize {
            self.rt
                .block_on(self.inner.flush_pending_commits_with_timeout(timeout))
        }

        /// Create a reservation (blocking).
        pub fn create_reservation(
            &self,
            req: &ReservationCreateRequest,
        ) -> Result<ReservationCreateResponse, Error> {
            self.rt.block_on(self.inner.create_reservation(req))
        }

        /// Create a reservation with response metadata (blocking).
        pub fn create_reservation_with_metadata(
            &self,
            req: &ReservationCreateRequest,
        ) -> Result<ApiResponse<ReservationCreateResponse>, Error> {
            self.rt
                .block_on(self.inner.create_reservation_with_metadata(req))
        }

        /// Commit a reservation (blocking).
        pub fn commit_reservation(
            &self,
            id: &ReservationId,
            req: &CommitRequest,
        ) -> Result<CommitResponse, Error> {
            self.rt.block_on(self.inner.commit_reservation(id, req))
        }

        /// Release a reservation (blocking).
        pub fn release_reservation(
            &self,
            id: &ReservationId,
            req: &ReleaseRequest,
        ) -> Result<ReleaseResponse, Error> {
            self.rt.block_on(self.inner.release_reservation(id, req))
        }

        /// Extend a reservation (blocking).
        pub fn extend_reservation(
            &self,
            id: &ReservationId,
            req: &ExtendRequest,
        ) -> Result<ExtendResponse, Error> {
            self.rt.block_on(self.inner.extend_reservation(id, req))
        }

        /// Preflight decision check (blocking).
        pub fn decide(&self, req: &DecisionRequest) -> Result<DecisionResponse, Error> {
            self.rt.block_on(self.inner.decide(req))
        }

        /// Create a direct-debit event (blocking).
        pub fn create_event(&self, req: &EventCreateRequest) -> Result<EventCreateResponse, Error> {
            self.rt.block_on(self.inner.create_event(req))
        }

        /// List reservations (blocking).
        pub fn list_reservations(
            &self,
            params: &ListReservationsParams,
        ) -> Result<ReservationListResponse, Error> {
            self.rt.block_on(self.inner.list_reservations(params))
        }

        /// Get a single reservation (blocking).
        pub fn get_reservation(&self, id: &ReservationId) -> Result<ReservationDetail, Error> {
            self.rt.block_on(self.inner.get_reservation(id))
        }

        /// Query balances (blocking).
        pub fn get_balances(&self, params: &BalanceParams) -> Result<BalanceResponse, Error> {
            self.rt.block_on(self.inner.get_balances(params))
        }
    }
}
