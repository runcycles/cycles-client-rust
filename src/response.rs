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
    /// The HTTP `Date` response header, as Unix milliseconds (second
    /// resolution). `None` when the header is absent or unparseable.
    ///
    /// Note the RFC 9110 caveats: `Date` is a whole-second, best-effort
    /// *origination* timestamp that intermediaries may replace, and it is
    /// generally **not** stamped by the same clock as body timestamps such
    /// as a reservation's `expires_at_ms` (in cycles-server those come from
    /// Redis `TIME` while `Date` comes from the HTTP layer). Treat
    /// cross-source arithmetic like `expires_at_ms − date_ms` as a rough
    /// estimate at best — the SDK itself derives no lease behavior from this
    /// field. Heartbeats use `remaining_ttl_ms` when present and the
    /// documented best-effort fallback when it is absent (see
    /// `src/heartbeat.rs`).
    pub date_ms: Option<u64>,
}

impl<T> ApiResponse<T> {
    /// Consume the wrapper and return the inner data.
    pub fn into_inner(self) -> T {
        self.data
    }

    /// Create a new `ApiResponse` from response data and HTTP headers.
    pub(crate) fn from_response(data: T, headers: &reqwest::header::HeaderMap) -> Self {
        let header_str = |name: &str| -> Option<String> {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(String::from)
        };
        let header_u32 = |name: &str| -> Option<u32> {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
        };
        let header_u64 = |name: &str| -> Option<u64> {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
        };

        Self {
            data,
            request_id: header_str("x-request-id"),
            rate_limit_remaining: header_u32("x-ratelimit-remaining"),
            rate_limit_reset: header_u64("x-ratelimit-reset"),
            cycles_tenant: header_str("x-cycles-tenant"),
            date_ms: headers
                .get("date")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_http_date_ms),
        }
    }
}

/// Parse an HTTP-date (RFC 9110 `Date` header value) into Unix milliseconds.
///
/// Returns `None` for garbage or pre-epoch dates — callers must treat a
/// missing sample as "unknown", never as zero.
pub(crate) fn parse_http_date_ms(value: &str) -> Option<u64> {
    let time = httpdate::parse_http_date(value).ok()?;
    let ms = time.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis();
    u64::try_from(ms).ok()
}

impl<T> std::ops::Deref for ApiResponse<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_imf_fixdate() {
        // 1_700_000_000 s = Tue, 14 Nov 2023 22:13:20 GMT.
        assert_eq!(
            parse_http_date_ms("Tue, 14 Nov 2023 22:13:20 GMT"),
            Some(1_700_000_000_000)
        );
    }

    #[test]
    fn garbage_and_empty_dates_are_none() {
        assert_eq!(parse_http_date_ms("not-a-date"), None);
        assert_eq!(parse_http_date_ms(""), None);
        assert_eq!(parse_http_date_ms("Tue, 99 Nov 2023 22:13:20 GMT"), None);
    }
}
