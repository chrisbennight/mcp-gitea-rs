use std::{env, fmt, time::Duration};

use thiserror::Error;

const DEFAULT_HOST: &str = "0.0.0.0";
const DEFAULT_PORT: u16 = 8000;
const DEFAULT_TIMEOUT_SECONDS: u64 = 30;
const MIN_TIMEOUT_SECONDS: u64 = 1;
const MAX_TIMEOUT_SECONDS: u64 = 300;
const DEFAULT_MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
const MIN_MAX_REQUEST_BYTES: usize = 1024;
const MAX_MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 8;
const MAX_MAX_CONCURRENT_REQUESTS: usize = 64;
const MAX_ALLOWED_HOSTS: usize = 32;
const MAX_HOST_CHARACTERS: usize = 255;

#[derive(Clone)]
pub struct TokenCredentials {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for TokenCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TokenCredentials([REDACTED])")
    }
}

#[derive(Clone)]
pub struct Settings {
    pub upstream_url: String,
    pub service_token: String,
    pub token_credentials: Option<TokenCredentials>,
    pub gateway_bearer_current: String,
    pub gateway_bearer_previous: Option<String>,
    pub host: String,
    pub port: u16,
    pub allowed_hosts: Vec<String>,
    pub allowed_origins: Vec<String>,
    pub timeout: Duration,
    pub body_timeout: Duration,
    pub max_request_bytes: usize,
    pub max_concurrent_requests: usize,
    pub file_public_origin: Option<String>,
    pub identity: Option<crate::identity::IdentityVerifier>,
    pub log_level: String,
}

impl fmt::Debug for Settings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Settings")
            .field("upstream_url", &self.upstream_url)
            .field("service_token", &"[REDACTED]")
            .field(
                "token_administration_configured",
                &self.token_credentials.is_some(),
            )
            .field("gateway_bearer_current", &"[REDACTED]")
            .field(
                "gateway_bearer_previous_configured",
                &self.gateway_bearer_previous.is_some(),
            )
            .field("host", &self.host)
            .field("identity_verification_configured", &self.identity.is_some())
            .field("port", &self.port)
            .field("allowed_hosts", &self.allowed_hosts)
            .field("allowed_origins", &self.allowed_origins)
            .field("timeout", &self.timeout)
            .field("body_timeout", &self.body_timeout)
            .field("max_request_bytes", &self.max_request_bytes)
            .field("max_concurrent_requests", &self.max_concurrent_requests)
            .field(
                "file_public_origin_configured",
                &self.file_public_origin.is_some(),
            )
            .field("log_level", &self.log_level)
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("{name} is required")]
    Missing { name: &'static str },
    #[error("{name} is invalid: {message}")]
    Invalid { name: &'static str, message: String },
}

impl Settings {
    /// Load and validate service configuration.
    ///
    /// # Errors
    ///
    /// Returns an error for missing credentials or malformed bounded values.
    pub fn from_env() -> Result<Self, SettingsError> {
        let upstream_url = required("GITEA_MCP_UPSTREAM_URL")?;
        let service_token = required("GITEA_MCP_SERVICE_TOKEN")?;
        let token_credentials = match (
            optional("GITEA_MCP_TOKEN_USERNAME"),
            optional("GITEA_MCP_TOKEN_PASSWORD"),
        ) {
            (None, None) => None,
            (Some(username), Some(password)) => Some(TokenCredentials { username, password }),
            _ => {
                return Err(SettingsError::Invalid {
                    name: "GITEA_MCP_TOKEN_USERNAME/GITEA_MCP_TOKEN_PASSWORD",
                    message: "configure both credentials or omit both".to_string(),
                });
            }
        };
        let gateway_bearer_current = required("GITEA_MCP_GATEWAY_BEARER_CURRENT")?;
        validate_bearer("GITEA_MCP_GATEWAY_BEARER_CURRENT", &gateway_bearer_current)?;
        let gateway_bearer_previous = env::var("GITEA_MCP_GATEWAY_BEARER_PREVIOUS")
            .ok()
            .filter(|value| !value.is_empty());
        if let Some(previous) = gateway_bearer_previous.as_deref() {
            validate_bearer("GITEA_MCP_GATEWAY_BEARER_PREVIOUS", previous)?;
        }
        let timeout_seconds = parsed_or("GITEA_MCP_HTTP_TIMEOUT_SECONDS", DEFAULT_TIMEOUT_SECONDS)?;
        if !(MIN_TIMEOUT_SECONDS..=MAX_TIMEOUT_SECONDS).contains(&timeout_seconds) {
            return Err(SettingsError::Invalid {
                name: "GITEA_MCP_HTTP_TIMEOUT_SECONDS",
                message: format!("must be between {MIN_TIMEOUT_SECONDS} and {MAX_TIMEOUT_SECONDS}"),
            });
        }
        let max_request_bytes =
            parsed_or("GITEA_MCP_MAX_REQUEST_BYTES", DEFAULT_MAX_REQUEST_BYTES)?;
        validate_max_request_bytes(max_request_bytes)?;
        let max_concurrent_requests = parsed_or(
            "GITEA_MCP_MAX_CONCURRENT_REQUESTS",
            DEFAULT_MAX_CONCURRENT_REQUESTS,
        )?;
        validate_max_concurrent_requests(max_concurrent_requests)?;
        let body_timeout_seconds =
            parsed_or("GITEA_MCP_BODY_TIMEOUT_SECONDS", DEFAULT_TIMEOUT_SECONDS)?;
        if !(MIN_TIMEOUT_SECONDS..=MAX_TIMEOUT_SECONDS).contains(&body_timeout_seconds) {
            return Err(SettingsError::Invalid {
                name: "GITEA_MCP_BODY_TIMEOUT_SECONDS",
                message: format!("must be between {MIN_TIMEOUT_SECONDS} and {MAX_TIMEOUT_SECONDS}"),
            });
        }
        let file_public_origin = env::var("GITEA_MCP_FILE_PUBLIC_ORIGIN")
            .ok()
            .filter(|value| !value.is_empty())
            .map(|origin| {
                gitea_mcp::files::validate_public_origin(&origin).map_err(|message| {
                    SettingsError::Invalid {
                        name: "GITEA_MCP_FILE_PUBLIC_ORIGIN",
                        message,
                    }
                })
            })
            .transpose()?;
        Ok(Self {
            identity: identity_from_env()?,
            upstream_url,
            service_token,
            token_credentials,
            gateway_bearer_current,
            gateway_bearer_previous,
            host: value_or("GITEA_MCP_HOST", DEFAULT_HOST),
            port: parsed_or("GITEA_MCP_PORT", DEFAULT_PORT)?,
            allowed_hosts: allowed_hosts()?,
            allowed_origins: parse_allowed_origins(&value_or("GITEA_MCP_ALLOWED_ORIGINS", ""))?,
            timeout: Duration::from_secs(timeout_seconds),
            body_timeout: Duration::from_secs(body_timeout_seconds),
            max_request_bytes,
            max_concurrent_requests,
            file_public_origin,
            log_level: value_or("GITEA_MCP_LOG_LEVEL", "info"),
        })
    }
}

fn identity_from_env() -> Result<Option<crate::identity::IdentityVerifier>, SettingsError> {
    let identity = match (
        optional("GITEA_MCP_IDENTITY_JWKS_URL"),
        optional("GITEA_MCP_IDENTITY_ISSUER"),
        optional("GITEA_MCP_IDENTITY_ACTOR"),
    ) {
        (None, None, None) => None,
        (Some(url), Some(issuer), Some(actor)) => Some(
            crate::identity::IdentityVerifier::new(&url, issuer, actor).map_err(|message| {
                SettingsError::Invalid {
                    name: "GITEA_MCP_IDENTITY_*",
                    message: message.to_string(),
                }
            })?,
        ),
        _ => {
            return Err(SettingsError::Invalid {
                name: "GITEA_MCP_IDENTITY_*",
                message: "configure JWKS_URL, ISSUER, and ACTOR together or omit all three"
                    .to_string(),
            });
        }
    };
    Ok(identity)
}

fn optional(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty())
}

fn required(name: &'static str) -> Result<String, SettingsError> {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or(SettingsError::Missing { name })
}

fn value_or(name: &str, default: &str) -> String {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn parsed_or<T>(name: &'static str, default: T) -> Result<T, SettingsError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match env::var(name) {
        Ok(value) if !value.is_empty() => {
            value
                .parse()
                .map_err(|error: T::Err| SettingsError::Invalid {
                    name,
                    message: error.to_string(),
                })
        }
        _ => Ok(default),
    }
}

fn allowed_hosts() -> Result<Vec<String>, SettingsError> {
    let raw = value_or("GITEA_MCP_ALLOWED_HOSTS", "localhost,127.0.0.1,::1");
    let hosts: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect();
    if hosts.is_empty()
        || hosts.len() > MAX_ALLOWED_HOSTS
        || hosts
            .iter()
            .any(|host| host.chars().count() > MAX_HOST_CHARACTERS)
    {
        return Err(SettingsError::Invalid {
            name: "GITEA_MCP_ALLOWED_HOSTS",
            message: format!("must contain 1 to {MAX_ALLOWED_HOSTS} bounded entries"),
        });
    }
    Ok(hosts)
}

fn parse_allowed_origins(raw: &str) -> Result<Vec<String>, SettingsError> {
    let invalid = || SettingsError::Invalid {
        name: "GITEA_MCP_ALLOWED_ORIGINS",
        message: "must contain at most 32 bare HTTP(S) origins, each at most 255 bytes".to_string(),
    };
    let entries: Vec<_> = raw
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .collect();
    if entries.len() > MAX_ALLOWED_HOSTS {
        return Err(invalid());
    }
    entries
        .into_iter()
        .map(|entry| {
            if entry.len() > MAX_HOST_CHARACTERS {
                return Err(invalid());
            }
            normalize_origin(entry).ok_or_else(invalid)
        })
        .collect()
}

pub(crate) fn normalize_origin(value: &str) -> Option<String> {
    let uri: axum::http::Uri = value.parse().ok()?;
    if uri
        .path_and_query()
        .is_some_and(|path| !matches!(path.as_str(), "" | "/"))
    {
        return None;
    }
    gitea_mcp::files::validate_public_origin(value).ok()
}

fn validate_bearer(name: &'static str, bearer: &str) -> Result<(), SettingsError> {
    if bearer.len() < 32 {
        return Err(SettingsError::Invalid {
            name,
            message: "must contain at least 32 bytes".to_string(),
        });
    }
    Ok(())
}

fn validate_max_request_bytes(max_request_bytes: usize) -> Result<(), SettingsError> {
    if !(MIN_MAX_REQUEST_BYTES..=MAX_MAX_REQUEST_BYTES).contains(&max_request_bytes) {
        return Err(SettingsError::Invalid {
            name: "GITEA_MCP_MAX_REQUEST_BYTES",
            message: format!("must be between {MIN_MAX_REQUEST_BYTES} and {MAX_MAX_REQUEST_BYTES}"),
        });
    }
    Ok(())
}

fn validate_max_concurrent_requests(max_concurrent_requests: usize) -> Result<(), SettingsError> {
    if !(1..=MAX_MAX_CONCURRENT_REQUESTS).contains(&max_concurrent_requests) {
        return Err(SettingsError::Invalid {
            name: "GITEA_MCP_MAX_CONCURRENT_REQUESTS",
            message: format!("must be between 1 and {MAX_MAX_CONCURRENT_REQUESTS}"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_are_explicit_bounded_http_origins() {
        assert_eq!(parse_allowed_origins("").unwrap(), Vec::<String>::new());
        assert_eq!(
            parse_allowed_origins("https://EXAMPLE.test:443,http://localhost:8080").unwrap(),
            ["https://example.test", "http://localhost:8080"]
        );
        for invalid in [
            "*",
            "null",
            "file:///tmp",
            "https://user:password@example.test",
            "https://example.test/path",
            "https://example.test/path/..",
            "https://example.test?x=y",
        ] {
            assert!(parse_allowed_origins(invalid).is_err());
        }
        assert!(parse_allowed_origins(&vec!["https://example.test"; 33].join(",")).is_err());
        assert!(parse_allowed_origins(&format!("https://{}.test", "a".repeat(256))).is_err());
    }

    #[test]
    fn rotation_bearers_share_the_same_minimum_strength() {
        assert!(validate_bearer("current", "0123456789abcdef0123456789abcdef").is_ok());
        assert!(validate_bearer("previous", "short").is_err());
    }

    #[test]
    fn request_body_limit_rejects_unbounded_configuration() {
        assert!(validate_max_request_bytes(DEFAULT_MAX_REQUEST_BYTES).is_ok());
        assert!(validate_max_request_bytes(MIN_MAX_REQUEST_BYTES - 1).is_err());
        assert!(validate_max_request_bytes(MAX_MAX_REQUEST_BYTES + 1).is_err());
    }

    #[test]
    fn concurrency_limit_rejects_unbounded_configuration() {
        assert!(validate_max_concurrent_requests(DEFAULT_MAX_CONCURRENT_REQUESTS).is_ok());
        assert!(validate_max_concurrent_requests(0).is_err());
        assert!(validate_max_concurrent_requests(MAX_MAX_CONCURRENT_REQUESTS + 1).is_err());
    }

    #[test]
    fn settings_diagnostics_redact_every_credential() {
        let settings = Settings {
            upstream_url: "https://gitea.example.test".to_string(),
            service_token: "visible-only-if-debug-is-unsafe".to_string(),
            token_credentials: Some(TokenCredentials {
                username: "visible-token-user".to_string(),
                password: "visible-token-password".to_string(),
            }),
            gateway_bearer_current: "current-visible-only-if-debug-is-unsafe".to_string(),
            gateway_bearer_previous: Some("previous-visible-only-if-debug-is-unsafe".to_string()),
            host: DEFAULT_HOST.to_string(),
            port: DEFAULT_PORT,
            allowed_hosts: vec!["localhost".to_string()],
            allowed_origins: Vec::new(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECONDS),
            body_timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECONDS),
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            max_concurrent_requests: DEFAULT_MAX_CONCURRENT_REQUESTS,
            file_public_origin: None,
            identity: None,
            log_level: "info".to_string(),
        };

        let diagnostic = format!("{settings:?}");
        assert!(!diagnostic.contains("visible-only-if-debug-is-unsafe"));
        assert!(!diagnostic.contains("visible-token-user"));
        assert!(!diagnostic.contains("visible-token-password"));
        assert!(diagnostic.contains("gateway_bearer_previous_configured: true"));
    }
}
