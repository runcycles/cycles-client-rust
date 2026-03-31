//! Strongly-typed newtype wrappers for protocol identifiers.

use serde::{Deserialize, Serialize};

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Create a new identifier from any string-like value.
            pub fn new(id: impl Into<String>) -> Self {
                Self(id.into())
            }

            /// Borrow the inner string.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Consume and return the inner string.
            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_owned())
            }
        }
    };
}

define_id!(
    /// Unique identifier for a budget reservation.
    ReservationId
);

define_id!(
    /// Idempotency key for safe request retries.
    IdempotencyKey
);

define_id!(
    /// Unique identifier for a direct-debit event.
    EventId
);

impl IdempotencyKey {
    /// Generate a new random idempotency key (UUID v4).
    pub fn random() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newtype_display_and_serde() {
        let id = ReservationId::new("rsv_123");
        assert_eq!(id.as_str(), "rsv_123");
        assert_eq!(id.to_string(), "rsv_123");

        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"rsv_123\"");

        let deserialized: ReservationId = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, id);
    }

    #[test]
    fn idempotency_key_random() {
        let k1 = IdempotencyKey::random();
        let k2 = IdempotencyKey::random();
        assert_ne!(k1, k2);
        assert_eq!(k1.as_str().len(), 36); // UUID v4 format
    }

    #[test]
    fn from_conversions() {
        let id: ReservationId = "rsv_abc".into();
        assert_eq!(id.as_str(), "rsv_abc");

        let id2: ReservationId = String::from("rsv_def").into();
        assert_eq!(id2.as_str(), "rsv_def");
    }

    #[test]
    fn into_inner() {
        let id = ReservationId::new("rsv_xyz");
        let inner: String = id.into_inner();
        assert_eq!(inner, "rsv_xyz");

        let ek = EventId::new("evt_123");
        let inner2: String = ek.into_inner();
        assert_eq!(inner2, "evt_123");
    }
}
