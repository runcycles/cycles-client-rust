//! Core value objects shared across requests and responses.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::enums::Unit;

/// A non-negative budget amount with a unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Amount {
    /// The unit of measurement.
    pub unit: Unit,
    /// The amount (non-negative).
    pub amount: i64,
}

impl Amount {
    /// Create an amount in USD microcents.
    pub fn usd_microcents(amount: i64) -> Self {
        Self {
            unit: Unit::UsdMicrocents,
            amount,
        }
    }

    /// Create an amount in tokens.
    pub fn tokens(amount: i64) -> Self {
        Self {
            unit: Unit::Tokens,
            amount,
        }
    }

    /// Create an amount in credits.
    pub fn credits(amount: i64) -> Self {
        Self {
            unit: Unit::Credits,
            amount,
        }
    }
}

/// A signed budget amount (can be negative for debt).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedAmount {
    /// The unit of measurement.
    pub unit: Unit,
    /// The amount (may be negative).
    pub amount: i64,
}

/// Subject identifies who is spending. At least one field must be set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    /// Top-level tenant identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// Workspace within the tenant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Application identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// Workflow identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// Agent identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Toolset identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toolset: Option<String>,
    /// Additional custom dimensions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<HashMap<String, String>>,
}

impl Subject {
    /// Returns `true` if at least one standard field is set.
    pub fn has_field(&self) -> bool {
        self.tenant.is_some()
            || self.workspace.is_some()
            || self.app.is_some()
            || self.workflow.is_some()
            || self.agent.is_some()
            || self.toolset.is_some()
    }
}

/// Action describes what is being done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action {
    /// The kind of action (e.g., "llm.completion").
    pub kind: String,
    /// The specific action name (e.g., "gpt-4o").
    pub name: String,
    /// Optional tags for categorization.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}

impl Action {
    /// Create a new action with kind and name.
    pub fn new(kind: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            name: name.into(),
            tags: None,
        }
    }
}

/// Soft constraints returned when the decision is `AllowWithCaps`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caps {
    /// Maximum tokens allowed for this operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i64>,
    /// Maximum remaining steps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_steps_remaining: Option<i64>,
    /// Only these tools are allowed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_allowlist: Option<Vec<String>>,
    /// These tools are denied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_denylist: Option<Vec<String>>,
    /// Cooldown period in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_ms: Option<i64>,
}

impl Caps {
    /// Check if a tool is allowed under these caps.
    ///
    /// - If an allowlist is set, the tool must be in it.
    /// - If a denylist is set, the tool must not be in it.
    /// - If neither is set, all tools are allowed.
    pub fn is_tool_allowed(&self, tool: &str) -> bool {
        if let Some(ref allowlist) = self.tool_allowlist {
            if !allowlist.is_empty() {
                return allowlist.iter().any(|t| t == tool);
            }
        }
        if let Some(ref denylist) = self.tool_denylist {
            if !denylist.is_empty() {
                return !denylist.iter().any(|t| t == tool);
            }
        }
        true
    }
}

/// Metrics about the guarded operation for observability.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CyclesMetrics {
    /// Number of input tokens consumed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_input: Option<i64>,
    /// Number of output tokens produced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_output: Option<i64>,
    /// Latency of the operation in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<i64>,
    /// Model version used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_version: Option<String>,
    /// Custom key-value metrics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom: Option<HashMap<String, serde_json::Value>>,
}

/// Budget balance for a scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Balance {
    /// The scope identifier.
    pub scope: String,
    /// The fully qualified scope path.
    pub scope_path: String,
    /// Remaining budget.
    pub remaining: SignedAmount,
    /// Currently reserved amount.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reserved: Option<Amount>,
    /// Total spent amount.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spent: Option<Amount>,
    /// Total allocated budget.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allocated: Option<Amount>,
    /// Current debt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debt: Option<Amount>,
    /// Overdraft limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overdraft_limit: Option<Amount>,
    /// Whether the scope is over its limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_over_limit: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amount_constructors() {
        let a = Amount::usd_microcents(5000);
        assert_eq!(a.unit, Unit::UsdMicrocents);
        assert_eq!(a.amount, 5000);

        let b = Amount::tokens(100);
        assert_eq!(b.unit, Unit::Tokens);
        assert_eq!(b.amount, 100);

        let c = Amount::credits(50);
        assert_eq!(c.unit, Unit::Credits);
        assert_eq!(c.amount, 50);
    }

    #[test]
    fn subject_has_field() {
        let empty = Subject::default();
        assert!(!empty.has_field());

        let with_tenant = Subject {
            tenant: Some("acme".to_string()),
            ..Default::default()
        };
        assert!(with_tenant.has_field());
    }

    #[test]
    fn caps_tool_allowed() {
        let caps = Caps {
            tool_allowlist: Some(vec!["web_search".to_string()]),
            ..Default::default()
        };
        assert!(caps.is_tool_allowed("web_search"));
        assert!(!caps.is_tool_allowed("code_exec"));

        let caps_deny = Caps {
            tool_denylist: Some(vec!["dangerous".to_string()]),
            ..Default::default()
        };
        assert!(caps_deny.is_tool_allowed("web_search"));
        assert!(!caps_deny.is_tool_allowed("dangerous"));

        let caps_empty = Caps::default();
        assert!(caps_empty.is_tool_allowed("anything"));
    }

    #[test]
    fn amount_serde_roundtrip() {
        let a = Amount::usd_microcents(5000);
        let json = serde_json::to_string(&a).unwrap();
        assert!(json.contains("\"USD_MICROCENTS\""));
        assert!(json.contains("5000"));
        let b: Amount = serde_json::from_str(&json).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn subject_serde_skips_none() {
        let s = Subject {
            tenant: Some("acme".to_string()),
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"tenant\""));
        assert!(!json.contains("\"workspace\""));
    }

    #[test]
    fn action_new() {
        let a = Action::new("llm.completion", "gpt-4o");
        assert_eq!(a.kind, "llm.completion");
        assert_eq!(a.name, "gpt-4o");
        assert!(a.tags.is_none());
    }
}
