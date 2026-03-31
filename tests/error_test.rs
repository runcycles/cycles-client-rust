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
    };
    assert!(err.is_budget_exceeded());
    assert_eq!(err.retry_after(), Some(Duration::from_millis(10000)));
    assert_eq!(err.request_id(), Some("req-budget"));
    assert_eq!(err.error_code(), Some(ErrorCode::BudgetExceeded));
    // Budget exceeded with retry_after is retryable
    assert!(err.is_retryable());
}

#[test]
fn budget_exceeded_without_retry_not_retryable() {
    let err = Error::BudgetExceeded {
        message: "Over budget".into(),
        affected_scopes: vec![],
        retry_after: None,
        request_id: None,
    };
    assert!(!err.is_retryable());
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
