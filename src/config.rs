//! Client configuration.

use std::time::Duration;

use crate::error::Error;

/// Configuration for the Cycles client.
#[derive(Debug, Clone)]
pub struct CyclesConfig {
    /// Base URL of the Cycles server (e.g., `http://localhost:7878`).
    pub base_url: String,
    /// API key for authentication.
    pub api_key: String,
    /// Default tenant for requests.
    pub tenant: Option<String>,
    /// Default workspace for requests.
    pub workspace: Option<String>,
    /// Default app for requests.
    pub app: Option<String>,
    /// Default workflow for requests.
    pub workflow: Option<String>,
    /// Default agent for requests.
    pub agent: Option<String>,
    /// Default toolset for requests.
    pub toolset: Option<String>,
    /// HTTP connect timeout.
    pub connect_timeout: Duration,
    /// HTTP read timeout.
    pub read_timeout: Duration,
    /// Whether commit retry is enabled.
    pub retry_enabled: bool,
    /// Maximum number of retry attempts.
    pub retry_max_attempts: u32,
    /// Initial delay between retries.
    pub retry_initial_delay: Duration,
    /// Multiplier for exponential backoff.
    pub retry_multiplier: f64,
    /// Maximum delay between retries.
    pub retry_max_delay: Duration,
}

impl CyclesConfig {
    /// Create a new config from environment variables.
    ///
    /// Required: `CYCLES_BASE_URL`, `CYCLES_API_KEY`.
    /// Optional: `CYCLES_TENANT`, `CYCLES_WORKSPACE`, `CYCLES_APP`, etc.
    pub fn from_env() -> Result<Self, Error> {
        Self::from_env_with_prefix("CYCLES_")
    }

    /// Create a new config from environment variables with a custom prefix.
    pub fn from_env_with_prefix(prefix: &str) -> Result<Self, Error> {
        let base_url = std::env::var(format!("{prefix}BASE_URL")).map_err(|_| {
            Error::Config(format!("{prefix}BASE_URL environment variable is required"))
        })?;
        let api_key = std::env::var(format!("{prefix}API_KEY")).map_err(|_| {
            Error::Config(format!("{prefix}API_KEY environment variable is required"))
        })?;

        let env_opt = |name: &str| std::env::var(format!("{prefix}{name}")).ok();
        let env_duration_ms = |name: &str, default_ms: u64| -> Duration {
            env_opt(name)
                .and_then(|v| v.parse::<u64>().ok())
                .map_or(Duration::from_millis(default_ms), Duration::from_millis)
        };
        let env_u32 = |name: &str, default: u32| -> u32 {
            env_opt(name)
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        };
        let env_f64 = |name: &str, default: f64| -> f64 {
            env_opt(name)
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        };

        Ok(Self {
            base_url,
            api_key,
            tenant: env_opt("TENANT"),
            workspace: env_opt("WORKSPACE"),
            app: env_opt("APP"),
            workflow: env_opt("WORKFLOW"),
            agent: env_opt("AGENT"),
            toolset: env_opt("TOOLSET"),
            connect_timeout: env_duration_ms("CONNECT_TIMEOUT", 2_000),
            read_timeout: env_duration_ms("READ_TIMEOUT", 5_000),
            retry_enabled: env_opt("RETRY_ENABLED").map_or(true, |v| v.to_lowercase() != "false"),
            retry_max_attempts: env_u32("RETRY_MAX_ATTEMPTS", 5),
            retry_initial_delay: env_duration_ms("RETRY_INITIAL_DELAY", 500),
            retry_multiplier: env_f64("RETRY_MULTIPLIER", 2.0),
            retry_max_delay: env_duration_ms("RETRY_MAX_DELAY", 30_000),
        })
    }
}

/// Builder for [`CyclesClient`](crate::CyclesClient).
///
/// # Example
///
/// ```rust,no_run
/// use runcycles::CyclesClient;
///
/// let client = CyclesClient::builder("my-api-key", "http://localhost:7878")
///     .tenant("acme")
///     .build();
/// ```
#[must_use = "builder does nothing until .build() is called"]
pub struct CyclesClientBuilder {
    config: CyclesConfig,
    http_client: Option<reqwest::Client>,
}

impl CyclesClientBuilder {
    /// Create a new builder with required parameters.
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            config: CyclesConfig {
                base_url: base_url.into(),
                api_key: api_key.into(),
                tenant: None,
                workspace: None,
                app: None,
                workflow: None,
                agent: None,
                toolset: None,
                connect_timeout: Duration::from_millis(2_000),
                read_timeout: Duration::from_millis(5_000),
                retry_enabled: true,
                retry_max_attempts: 5,
                retry_initial_delay: Duration::from_millis(500),
                retry_multiplier: 2.0,
                retry_max_delay: Duration::from_secs(30),
            },
            http_client: None,
        }
    }

    /// Set the default tenant for requests.
    pub fn tenant(mut self, tenant: impl Into<String>) -> Self {
        self.config.tenant = Some(tenant.into());
        self
    }

    /// Set the default workspace for requests.
    pub fn workspace(mut self, workspace: impl Into<String>) -> Self {
        self.config.workspace = Some(workspace.into());
        self
    }

    /// Set the default app for requests.
    pub fn app(mut self, app: impl Into<String>) -> Self {
        self.config.app = Some(app.into());
        self
    }

    /// Set the default workflow for requests.
    pub fn workflow(mut self, workflow: impl Into<String>) -> Self {
        self.config.workflow = Some(workflow.into());
        self
    }

    /// Set the default agent for requests.
    pub fn agent(mut self, agent: impl Into<String>) -> Self {
        self.config.agent = Some(agent.into());
        self
    }

    /// Set the default toolset for requests.
    pub fn toolset(mut self, toolset: impl Into<String>) -> Self {
        self.config.toolset = Some(toolset.into());
        self
    }

    /// Set the HTTP connect timeout.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.config.connect_timeout = timeout;
        self
    }

    /// Set the HTTP read timeout.
    pub fn read_timeout(mut self, timeout: Duration) -> Self {
        self.config.read_timeout = timeout;
        self
    }

    /// Enable or disable commit retry.
    pub fn retry_enabled(mut self, enabled: bool) -> Self {
        self.config.retry_enabled = enabled;
        self
    }

    /// Set the maximum number of retry attempts.
    pub fn retry_max_attempts(mut self, max: u32) -> Self {
        self.config.retry_max_attempts = max;
        self
    }

    /// Provide a pre-configured `reqwest::Client` for connection pool sharing.
    pub fn http_client(mut self, client: reqwest::Client) -> Self {
        self.http_client = Some(client);
        self
    }

    /// Build the [`CyclesClient`](crate::CyclesClient).
    pub fn build(self) -> crate::CyclesClient {
        crate::CyclesClient::from_builder(self.config, self.http_client)
    }
}
