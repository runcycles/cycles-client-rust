//! Response wrapper with metadata from HTTP headers.

/// Wraps a typed API response with metadata extracted from HTTP headers.
///
/// Use the high-level client methods (e.g., `create_reservation`) to get just `T`.
/// Use the `_with_metadata` variants to get `ApiResponse<T>` when you need
/// request IDs or rate limit information.
#[derive(Debug)]
pub struct ApiResponse<T> {
    /// The deserialized response body.
    pub data: T,
    /// Server-assigned request ID.
    pub request_id: Option<String>,
    /// Remaining rate limit quota.
    pub rate_limit_remaining: Option<u32>,
    /// When the rate limit resets (Unix seconds).
    pub rate_limit_reset: Option<u64>,
    /// Tenant from the response headers.
    pub cycles_tenant: Option<String>,
}

impl<T> ApiResponse<T> {
    /// Consume the wrapper and return the inner data.
    pub fn into_inner(self) -> T {
        self.data
    }

    /// Create a new `ApiResponse` from response data and HTTP headers.
    pub(crate) fn from_response(data: T, headers: &reqwest::header::HeaderMap) -> Self {
        let header_str = |name: &str| -> Option<String> {
            headers.get(name).and_then(|v| v.to_str().ok()).map(String::from)
        };
        let header_u32 = |name: &str| -> Option<u32> {
            headers.get(name).and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok())
        };
        let header_u64 = |name: &str| -> Option<u64> {
            headers.get(name).and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok())
        };

        Self {
            data,
            request_id: header_str("x-request-id"),
            rate_limit_remaining: header_u32("x-ratelimit-remaining"),
            rate_limit_reset: header_u64("x-ratelimit-reset"),
            cycles_tenant: header_str("x-cycles-tenant"),
        }
    }
}

impl<T> std::ops::Deref for ApiResponse<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.data
    }
}
