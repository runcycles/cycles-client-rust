//! Synchronous (blocking) wrapper for the Cycles client.
//!
//! This module is only available when the `blocking` feature is enabled.
//! It wraps the async [`CyclesClient`](crate::CyclesClient) with an internal
//! tokio runtime, following the same pattern as `reqwest::blocking`.

#[cfg(feature = "blocking")]
pub mod sync_client {
    use crate::config::{CyclesClientBuilder, CyclesConfig};
    use crate::error::Error;
    use crate::models::request::*;
    use crate::models::response::*;
    use crate::models::ReservationId;

    /// Synchronous (blocking) client for the Cycles API.
    ///
    /// Wraps the async client with an internal tokio runtime.
    pub struct BlockingCyclesClient {
        inner: crate::CyclesClient,
        rt: tokio::runtime::Runtime,
    }

    impl BlockingCyclesClient {
        /// Create a new blocking client builder.
        pub fn builder(
            api_key: impl Into<String>,
            base_url: impl Into<String>,
        ) -> CyclesClientBuilder {
            CyclesClientBuilder::new(api_key, base_url)
        }

        /// Create a blocking client from a config.
        pub fn new(config: CyclesConfig) -> Result<Self, Error> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| Error::Config(format!("failed to create tokio runtime: {e}")))?;
            let inner = crate::CyclesClient::new(config);
            Ok(Self { inner, rt })
        }

        /// Create a reservation (blocking).
        pub fn create_reservation(
            &self,
            req: &ReservationCreateRequest,
        ) -> Result<ReservationCreateResponse, Error> {
            self.rt.block_on(self.inner.create_reservation(req))
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
