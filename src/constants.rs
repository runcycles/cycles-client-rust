//! Internal constants for the Cycles client.

/// HTTP header name for the API key.
pub const API_KEY_HEADER: &str = "X-Cycles-API-Key";

/// HTTP header name for idempotency keys.
pub const IDEMPOTENCY_KEY_HEADER: &str = "X-Idempotency-Key";

/// Minimum allowed TTL in milliseconds.
pub const MIN_TTL_MS: u64 = 1_000;

/// Maximum allowed TTL in milliseconds (24 hours).
pub const MAX_TTL_MS: u64 = 86_400_000;

/// Maximum grace period in milliseconds.
pub const MAX_GRACE_PERIOD_MS: u64 = 60_000;

/// Maximum extend-by value in milliseconds (24 hours).
pub const MAX_EXTEND_BY_MS: u64 = 86_400_000;

/// API path for reservations.
pub const RESERVATIONS_PATH: &str = "/v1/reservations";

/// API path for decide (preflight).
pub const DECIDE_PATH: &str = "/v1/decide";

/// API path for balances.
pub const BALANCES_PATH: &str = "/v1/balances";

/// API path for events (direct debit).
pub const EVENTS_PATH: &str = "/v1/events";
