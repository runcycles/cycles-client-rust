//! Tests for CyclesConfig and CyclesClientBuilder.

use runcycles::{CyclesClient, CyclesConfig};
use std::time::Duration;

#[test]
fn builder_creates_client_with_defaults() {
    let client = CyclesClient::builder("my-key", "http://localhost:7878").build();
    let config = client.config();
    assert_eq!(config.base_url, "http://localhost:7878");
    assert_eq!(config.api_key, "my-key");
    assert_eq!(config.connect_timeout, Duration::from_millis(2000));
    assert_eq!(config.read_timeout, Duration::from_millis(5000));
    assert!(config.retry_enabled);
    assert_eq!(config.retry_max_attempts, 5);
    assert_eq!(config.retry_multiplier, 2.0);
    assert!(config.tenant.is_none());
}

#[test]
fn builder_sets_all_fields() {
    let client = CyclesClient::builder("my-key", "http://example.com")
        .tenant("acme")
        .workspace("prod")
        .app("my-app")
        .workflow("refund-flow")
        .agent("planner")
        .toolset("search-tools")
        .connect_timeout(Duration::from_secs(1))
        .read_timeout(Duration::from_secs(3))
        .retry_enabled(false)
        .retry_max_attempts(3)
        .build();

    let config = client.config();
    assert_eq!(config.tenant.as_deref(), Some("acme"));
    assert_eq!(config.workspace.as_deref(), Some("prod"));
    assert_eq!(config.app.as_deref(), Some("my-app"));
    assert_eq!(config.workflow.as_deref(), Some("refund-flow"));
    assert_eq!(config.agent.as_deref(), Some("planner"));
    assert_eq!(config.toolset.as_deref(), Some("search-tools"));
    assert_eq!(config.connect_timeout, Duration::from_secs(1));
    assert_eq!(config.read_timeout, Duration::from_secs(3));
    assert!(!config.retry_enabled);
    assert_eq!(config.retry_max_attempts, 3);
}

#[test]
fn builder_accepts_custom_http_client() {
    let http = reqwest::Client::new();
    let client = CyclesClient::builder("key", "http://localhost:7878")
        .http_client(http)
        .build();
    // Just verify it doesn't panic
    assert_eq!(client.config().base_url, "http://localhost:7878");
}

#[test]
fn client_is_clone() {
    let client = CyclesClient::builder("key", "http://localhost:7878").build();
    let clone = client.clone();
    assert_eq!(clone.config().base_url, client.config().base_url);
}

#[test]
fn client_is_debug() {
    let client = CyclesClient::builder("key", "http://localhost:7878").build();
    let debug = format!("{:?}", client);
    assert!(debug.contains("CyclesClient"));
    assert!(debug.contains("localhost"));
}

#[test]
fn config_from_env() {
    // Set required vars
    std::env::set_var("TEST_CYCLES_BASE_URL", "http://test:7878");
    std::env::set_var("TEST_CYCLES_API_KEY", "test-key-123");
    std::env::set_var("TEST_CYCLES_TENANT", "test-tenant");
    std::env::set_var("TEST_CYCLES_RETRY_ENABLED", "false");
    std::env::set_var("TEST_CYCLES_RETRY_MAX_ATTEMPTS", "3");
    std::env::set_var("TEST_CYCLES_CONNECT_TIMEOUT", "1000");
    std::env::set_var("TEST_CYCLES_READ_TIMEOUT", "3000");

    let config = CyclesConfig::from_env_with_prefix("TEST_CYCLES_").unwrap();

    assert_eq!(config.base_url, "http://test:7878");
    assert_eq!(config.api_key, "test-key-123");
    assert_eq!(config.tenant.as_deref(), Some("test-tenant"));
    assert!(!config.retry_enabled);
    assert_eq!(config.retry_max_attempts, 3);
    assert_eq!(config.connect_timeout, Duration::from_millis(1000));
    assert_eq!(config.read_timeout, Duration::from_millis(3000));

    // Cleanup
    std::env::remove_var("TEST_CYCLES_BASE_URL");
    std::env::remove_var("TEST_CYCLES_API_KEY");
    std::env::remove_var("TEST_CYCLES_TENANT");
    std::env::remove_var("TEST_CYCLES_RETRY_ENABLED");
    std::env::remove_var("TEST_CYCLES_RETRY_MAX_ATTEMPTS");
    std::env::remove_var("TEST_CYCLES_CONNECT_TIMEOUT");
    std::env::remove_var("TEST_CYCLES_READ_TIMEOUT");
}

#[test]
fn config_from_env_default_prefix() {
    // Test from_env() which calls from_env_with_prefix("CYCLES_")
    // Will fail because CYCLES_BASE_URL is not set
    let result = CyclesConfig::from_env();
    assert!(result.is_err());
}

#[test]
fn config_from_env_missing_base_url() {
    let result = CyclesConfig::from_env_with_prefix("NONEXISTENT_PREFIX_");
    assert!(result.is_err());
    let err = result.unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("BASE_URL"));
}

#[test]
fn config_from_env_missing_api_key() {
    std::env::set_var("MISS_KEY_BASE_URL", "http://test:7878");
    let result = CyclesConfig::from_env_with_prefix("MISS_KEY_");
    assert!(result.is_err());
    let err = result.unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("API_KEY"));
    std::env::remove_var("MISS_KEY_BASE_URL");
}

#[test]
fn new_client_from_config() {
    let config = CyclesConfig {
        base_url: "http://localhost:7878".into(),
        api_key: "test-key".into(),
        tenant: Some("acme".into()),
        workspace: None,
        app: None,
        workflow: None,
        agent: None,
        toolset: None,
        connect_timeout: Duration::from_secs(2),
        read_timeout: Duration::from_secs(5),
        retry_enabled: true,
        retry_max_attempts: 5,
        retry_initial_delay: Duration::from_millis(500),
        retry_multiplier: 2.0,
        retry_max_delay: Duration::from_secs(30),
    };

    let client = CyclesClient::new(config);
    assert_eq!(client.config().tenant.as_deref(), Some("acme"));
}
