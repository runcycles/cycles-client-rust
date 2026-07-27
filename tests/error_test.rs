//! Tests for Error type methods.

use runcycles::models::ErrorCode;
use runcycles::Error;
use std::time::Duration;

#[test]
fn transport_error_is_retryable() {
    // We can't easily construct a reqwest::Error, but we test the from impl
    // indirectly via the client tests. Here test the other error variants.
    let err = Error::Api {
        status: 500,
        code: Some(ErrorCode::InternalError),
        message: "Internal".into(),
        request_id: Some("req-1".into()),
        retry_after: None,
        details: None,
    };
    assert!(err.is_retryable());
    assert!(!err.is_budget_exceeded());
    assert_eq!(err.request_id(), Some("req-1"));
    assert_eq!(err.error_code(), Some(ErrorCode::InternalError));
}

#[test]
fn api_error_400_not_retryable() {
    let err = Error::Api {
        status: 400,
        code: Some(ErrorCode::InvalidRequest),
        message: "Bad request".into(),
        request_id: None,
        retry_after: None,
        details: None,
    };
    assert!(!err.is_retryable());
    assert!(err.request_id().is_none());
}

#[test]
fn api_error_unknown_code_is_retryable() {
    let err = Error::Api {
        status: 503,
        code: Some(ErrorCode::Unknown),
        message: "Unavailable".into(),
        request_id: None,
        retry_after: Some(Duration::from_secs(5)),
        details: None,
    };
    assert!(err.is_retryable());
    assert_eq!(err.retry_after(), Some(Duration::from_secs(5)));
}

#[test]
fn budget_exceeded_error() {
    let err = Error::BudgetExceeded {
        message: "Over budget".into(),
        affected_scopes: vec!["tenant:acme".into()],
        retry_after: Some(Duration::from_millis(10000)),
        request_id: Some("req-budget".into()),
        status: Some(409),
    };
    assert!(err.is_budget_exceeded());
    assert_eq!(err.retry_after(), Some(Duration::from_millis(10000)));
    assert_eq!(err.request_id(), Some("req-budget"));
    assert_eq!(err.error_code(), Some(ErrorCode::BudgetExceeded));
    assert_eq!(err.status(), Some(409));
    // A 409 BUDGET_EXCEEDED is a budget fact, not a transient fault:
    // NOT retryable even when the server suggests a retry delay.
    assert!(!err.is_retryable());
}

#[test]
fn budget_exceeded_without_retry_not_retryable() {
    let err = Error::BudgetExceeded {
        message: "Over budget".into(),
        affected_scopes: vec![],
        retry_after: None,
        request_id: None,
        status: Some(409),
    };
    assert!(!err.is_retryable());
}

#[test]
fn budget_exceeded_retryable_only_on_429_with_retry_after() {
    // Retryability requires BOTH a 429 status AND a retry delay. This
    // combination is dormant today (the 409-reclassification is the only
    // path constructing BudgetExceeded from an error response), but the
    // invariant is pinned so a future construction site cannot silently
    // widen retry behavior.
    let retryable = Error::BudgetExceeded {
        message: "Over budget".into(),
        affected_scopes: vec![],
        retry_after: Some(Duration::from_secs(1)),
        request_id: None,
        status: Some(429),
    };
    assert!(retryable.is_retryable());

    // 429 without a delay: not retryable.
    let no_delay = Error::BudgetExceeded {
        message: "Over budget".into(),
        affected_scopes: vec![],
        retry_after: None,
        request_id: None,
        status: Some(429),
    };
    assert!(!no_delay.is_retryable());

    // DENY-decision-derived (no HTTP error status) with a delay: not
    // retryable — the delay is advisory, the denial is a budget fact.
    let deny_derived = Error::BudgetExceeded {
        message: "Over budget".into(),
        affected_scopes: vec![],
        retry_after: Some(Duration::from_secs(1)),
        request_id: None,
        status: None,
    };
    assert!(!deny_derived.is_retryable());
    assert_eq!(deny_derived.status(), None);
}

#[test]
fn api_429_is_retryable_by_status_alone() {
    // Cross-SDK parity: a 429 whose body was absent or unparseable (no
    // typed error code) is still retryable — the status is authoritative.
    let err = Error::Api {
        status: 429,
        code: None,
        message: "HTTP 429".into(),
        request_id: None,
        retry_after: Some(Duration::from_secs(2)),
        details: None,
    };
    assert!(err.is_retryable());
    assert_eq!(err.retry_after(), Some(Duration::from_secs(2)));
    assert_eq!(err.status(), Some(429));

    // Even an unrelated (non-retryable) parsed code doesn't defeat the
    // status-based classification.
    let err = Error::Api {
        status: 429,
        code: Some(ErrorCode::InvalidRequest),
        message: "mislabeled".into(),
        request_id: None,
        retry_after: None,
        details: None,
    };
    assert!(err.is_retryable());
}

#[test]
fn status_accessor_none_for_non_http_errors() {
    assert_eq!(Error::Validation("bad".into()).status(), None);
    assert_eq!(Error::Config("bad".into()).status(), None);
}

#[test]
fn validation_error_not_retryable() {
    let err = Error::Validation("bad input".into());
    assert!(!err.is_retryable());
    assert!(!err.is_budget_exceeded());
    assert!(err.retry_after().is_none());
    assert!(err.request_id().is_none());
    assert!(err.error_code().is_none());
}

#[test]
fn config_error_not_retryable() {
    let err = Error::Config("bad config".into());
    assert!(!err.is_retryable());
}

#[test]
fn deserialization_error_not_retryable() {
    let serde_err: serde_json::Error = serde_json::from_str::<String>("not json").unwrap_err();
    let err = Error::Deserialization(serde_err);
    assert!(!err.is_retryable());
    assert!(err.error_code().is_none());
}

#[test]
fn error_display() {
    let err = Error::BudgetExceeded {
        message: "Over budget".into(),
        affected_scopes: vec![],
        retry_after: None,
        request_id: None,
        status: Some(409),
    };
    let display = format!("{err}");
    assert!(display.contains("budget exceeded"));
    assert!(display.contains("Over budget"));

    let err2 = Error::Api {
        status: 404,
        code: Some(ErrorCode::NotFound),
        message: "Not found".into(),
        request_id: None,
        retry_after: None,
        details: None,
    };
    let display2 = format!("{err2}");
    assert!(display2.contains("404"));
    assert!(display2.contains("Not found"));
}

#[test]
fn api_error_is_budget_exceeded_via_code() {
    let err = Error::Api {
        status: 409,
        code: Some(ErrorCode::BudgetExceeded),
        message: "Budget exceeded".into(),
        request_id: None,
        retry_after: None,
        details: None,
    };
    assert!(err.is_budget_exceeded());
}

#[test]
fn api_error_is_tenant_closed_via_code() {
    // Runtime spec v0.1.25.13: HTTP 409 TENANT_CLOSED when the owning
    // tenant is CLOSED (mirrors governance spec Rule 2).
    let err = Error::Api {
        status: 409,
        code: Some(ErrorCode::TenantClosed),
        message: "Owning tenant is CLOSED".into(),
        request_id: Some("req-tc".into()),
        retry_after: None,
        details: None,
    };
    assert!(err.is_tenant_closed());
    assert!(!err.is_budget_exceeded());
    assert!(!err.is_retryable());
    assert_eq!(err.error_code(), Some(ErrorCode::TenantClosed));
}

#[test]
fn api_error_limit_exceeded_is_retryable() {
    // Runtime spec v0.1.25.12: HTTP 429 rate limiting carries
    // error=LIMIT_EXCEEDED and is transient — retry after the
    // indicated delay.
    let err = Error::Api {
        status: 429,
        code: Some(ErrorCode::LimitExceeded),
        message: "Rate limited".into(),
        request_id: Some("req-rl".into()),
        retry_after: Some(Duration::from_secs(3)),
        details: None,
    };
    assert!(err.is_retryable());
    assert!(!err.is_tenant_closed());
    assert!(!err.is_budget_exceeded());
    assert_eq!(err.error_code(), Some(ErrorCode::LimitExceeded));
    assert_eq!(err.retry_after(), Some(Duration::from_secs(3)));
}

#[test]
fn tenant_closed_helper_false_for_other_variants() {
    let err = Error::Validation("bad input".into());
    assert!(!err.is_tenant_closed());

    let budget = Error::BudgetExceeded {
        message: "Over budget".into(),
        affected_scopes: vec![],
        retry_after: None,
        request_id: None,
        status: Some(409),
    };
    assert!(!budget.is_tenant_closed());
}

#[test]
fn commit_recovery_failed_carries_both_errors_and_is_final() {
    let err = Error::CommitRecoveryFailed {
        reservation_id: "rsv_1".into(),
        commit_error: Box::new(Error::Api {
            status: 409,
            code: Some(ErrorCode::ReservationExpired),
            message: "Reservation expired".into(),
            request_id: Some("req-c".into()),
            retry_after: None,
            details: None,
        }),
        event_error: Box::new(Error::Api {
            status: 500,
            code: Some(ErrorCode::InternalError),
            message: "Server error".into(),
            request_id: Some("req-e".into()),
            retry_after: None,
            details: None,
        }),
    };

    // Final by construction: commit retry and event fallback both ran out.
    assert!(!err.is_retryable());
    assert!(err.retry_after().is_none());

    // Display names the reservation and surfaces both underlying errors.
    let msg = err.to_string();
    assert!(msg.contains("rsv_1"));
    assert!(msg.contains("NOT recorded"));
    assert!(msg.contains("Reservation expired"));
    assert!(msg.contains("Server error"));

    // The underlying errors stay programmatically reachable.
    if let Error::CommitRecoveryFailed {
        commit_error,
        event_error,
        ..
    } = &err
    {
        assert_eq!(
            commit_error.error_code(),
            Some(ErrorCode::ReservationExpired)
        );
        assert_eq!(event_error.error_code(), Some(ErrorCode::InternalError));
    } else {
        unreachable!();
    }
}

#[test]
fn auth_errors_are_distinct_and_non_retryable() {
    for (status, code) in [(401, ErrorCode::Unauthorized), (403, ErrorCode::Forbidden)] {
        let err = Error::Api {
            status,
            code: Some(code),
            message: "denied".into(),
            request_id: None,
            retry_after: None,
            details: None,
        };
        assert!(err.is_auth_error(), "HTTP {status} must be an auth error");
        assert!(
            !err.is_retryable(),
            "auth failures are deterministic; retrying cannot succeed"
        );
        // The message identifies the HTTP status so callers can act.
        assert!(err.to_string().contains(&status.to_string()));
    }

    // Status alone is enough, even without a parsed code.
    let no_code = Error::Api {
        status: 401,
        code: None,
        message: "denied".into(),
        request_id: None,
        retry_after: None,
        details: None,
    };
    assert!(no_code.is_auth_error());

    let not_auth = Error::Api {
        status: 500,
        code: Some(ErrorCode::InternalError),
        message: "boom".into(),
        request_id: None,
        retry_after: None,
        details: None,
    };
    assert!(!not_auth.is_auth_error());
    assert!(!Error::Validation("bad".into()).is_auth_error());
}
