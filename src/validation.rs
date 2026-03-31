//! Input validation utilities.

use crate::constants::{MAX_EXTEND_BY_MS, MAX_GRACE_PERIOD_MS, MAX_TTL_MS, MIN_TTL_MS};
use crate::error::Error;
use crate::models::Subject;

/// Validate that a subject has at least one standard field set.
pub fn validate_subject(subject: &Subject) -> Result<(), Error> {
    if !subject.has_field() {
        return Err(Error::Validation(
            "Subject must have at least one standard field (tenant, workspace, app, workflow, agent, or toolset)".to_string(),
        ));
    }
    Ok(())
}

/// Validate that a TTL is within the allowed range (1s to 24h).
pub fn validate_ttl_ms(ttl_ms: u64) -> Result<(), Error> {
    if !(MIN_TTL_MS..=MAX_TTL_MS).contains(&ttl_ms) {
        return Err(Error::Validation(format!(
            "ttl_ms must be between {MIN_TTL_MS} and {MAX_TTL_MS}, got {ttl_ms}"
        )));
    }
    Ok(())
}

/// Validate that a grace period is within the allowed range.
pub fn validate_grace_period_ms(grace_period_ms: Option<u64>) -> Result<(), Error> {
    if let Some(gp) = grace_period_ms {
        if gp > MAX_GRACE_PERIOD_MS {
            return Err(Error::Validation(format!(
                "grace_period_ms must be between 0 and {MAX_GRACE_PERIOD_MS}, got {gp}"
            )));
        }
    }
    Ok(())
}

/// Validate that an extend-by value is within the allowed range.
pub fn validate_extend_by_ms(extend_by_ms: u64) -> Result<(), Error> {
    if !(1..=MAX_EXTEND_BY_MS).contains(&extend_by_ms) {
        return Err(Error::Validation(format!(
            "extend_by_ms must be between 1 and {MAX_EXTEND_BY_MS}, got {extend_by_ms}"
        )));
    }
    Ok(())
}

/// Validate that a numeric value is non-negative.
pub fn validate_non_negative(value: i64, name: &str) -> Result<(), Error> {
    if value < 0 {
        return Err(Error::Validation(format!(
            "{name} must be non-negative, got {value}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_with_no_fields_fails() {
        let s = Subject::default();
        assert!(validate_subject(&s).is_err());
    }

    #[test]
    fn subject_with_tenant_passes() {
        let s = Subject {
            tenant: Some("acme".to_string()),
            ..Default::default()
        };
        assert!(validate_subject(&s).is_ok());
    }

    #[test]
    fn ttl_boundaries() {
        assert!(validate_ttl_ms(999).is_err());
        assert!(validate_ttl_ms(1_000).is_ok());
        assert!(validate_ttl_ms(86_400_000).is_ok());
        assert!(validate_ttl_ms(86_400_001).is_err());
    }

    #[test]
    fn grace_period_boundaries() {
        assert!(validate_grace_period_ms(None).is_ok());
        assert!(validate_grace_period_ms(Some(0)).is_ok());
        assert!(validate_grace_period_ms(Some(60_000)).is_ok());
        assert!(validate_grace_period_ms(Some(60_001)).is_err());
    }

    #[test]
    fn extend_by_boundaries() {
        assert!(validate_extend_by_ms(0).is_err());
        assert!(validate_extend_by_ms(1).is_ok());
        assert!(validate_extend_by_ms(86_400_000).is_ok());
        assert!(validate_extend_by_ms(86_400_001).is_err());
    }

    #[test]
    fn non_negative() {
        assert!(validate_non_negative(0, "x").is_ok());
        assert!(validate_non_negative(100, "x").is_ok());
        assert!(validate_non_negative(-1, "x").is_err());
    }
}
