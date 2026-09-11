use std::{fmt, sync::Arc, time::Duration};

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

pub mod catalog;
mod execute;
mod token;

pub use execute::{OperationResponse, validate_path_segment};
pub use token::{
    AccessTokenCreated, AccessTokenMetadata, AccessTokenSelector, CreateAccessToken,
    TokenLifecycleClient, validate_create_access_token,
};

const API_PREFIX: &str = "api/v1/";
const MAX_VERSION_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_VERSION_CHARACTERS: usize = 128;
pub(crate) const MAX_ERROR_RESPONSE_BYTES: usize = 64 * 1024;
pub(crate) const MAX_ERROR_MESSAGE_CHARACTERS: usize = 2 * 1024;
/// Used whenever a refusal body is absent, unreadable, or cannot be trusted to
/// redact — never an empty reason, which would read as though nothing happened.
pub(crate) const GENERIC_REFUSAL: &str = "Gitea rejected the operation";

/// Read a refusal body and reduce it to the sentence Gitea meant to say.
///
/// Bounded on the way in, because an error body is still attacker-influenceable
/// input. A body that cannot be read at all yields the generic sentence rather
/// than an error: losing the reason must not also lose the status.
///
/// Any body that ends early — cut by the size cap or by a failed read — is
/// discarded rather than reduced. Redaction matches complete occurrences only,
/// so a credential straddling the cut leaves a prefix nothing can scrub. Both
/// abnormal terminations are treated alike deliberately; they differ in cause
/// and not in what they leave behind.
///
/// `secrets` are removed before the message is truncated; see
/// [`reduce_error_message`] for why that order matters.
pub(crate) async fn read_error_message(
    mut response: reqwest::Response,
    secrets: &[String],
) -> String {
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(None) => break,
            Ok(Some(chunk)) => {
                if body.len().saturating_add(chunk.len()) > MAX_ERROR_RESPONSE_BYTES {
                    // Cut by the cap: the same partial body an interrupted read
                    // leaves, and discarded for the same reason. No test pins
                    // this one, because a credential straddling a 64 KiB cut
                    // cannot survive a 2 KiB truncation that keeps the head of
                    // the message — the two branches are treated alike so that a
                    // later change to either bound cannot open the gap.
                    return GENERIC_REFUSAL.to_string();
                }
                body.extend_from_slice(&chunk);
            }
            Err(_) => return GENERIC_REFUSAL.to_string(),
        }
    }
    reduce_error_message(content_type.as_deref(), &body, secrets)
}

/// Reduce a refusal body to a bounded, redacted sentence.
///
/// Redaction runs before truncation, and that order is why this function
/// exists. Redaction replaces complete occurrences of a secret, so a credential
/// straddling the truncation boundary would no longer match and its surviving
/// prefix would reach the caller. Truncating last means whatever is cut has
/// already been scrubbed.
pub(crate) fn reduce_error_message(
    content_type: Option<&str>,
    body: &[u8],
    secrets: &[String],
) -> String {
    let message = redact(extract_error_message(content_type, body), secrets);
    message.chars().take(MAX_ERROR_MESSAGE_CHARACTERS).collect()
}

/// Pull the human-readable reason out of a refusal body, unbounded.
///
/// Callers go through [`reduce_error_message`]; this is separate only so that
/// redaction sees the whole message.
fn extract_error_message(content_type: Option<&str>, body: &[u8]) -> String {
    // HTTP media types are case-insensitive, so `Application/JSON` is JSON. A
    // case-sensitive check would send it down the raw branch and make the whole
    // serialized document the message.
    let media_type = content_type.map(str::to_ascii_lowercase);
    if media_type
        .as_deref()
        .is_some_and(|value| value.contains("application/json") || value.contains("+json"))
    {
        serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|value| {
                value
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
    } else {
        std::str::from_utf8(body).ok().map(str::to_string)
    }
    .filter(|message| !message.trim().is_empty())
    .unwrap_or_else(|| GENERIC_REFUSAL.to_string())
}

/// Replace every occurrence of `values` in `message`.
///
/// Each value is matched both as given and as JSON would serialize it, because a
/// credential containing a quote or a backslash appears escaped inside a
/// document reproduced verbatim, and the raw form would not match there.
///
/// Longest first, so that a value containing another is not partly revealed by
/// redacting the shorter one inside it.
pub(crate) fn redact(message: String, values: &[String]) -> String {
    let mut ordered: Vec<String> = values
        .iter()
        .filter(|value| !value.is_empty())
        .flat_map(|value| {
            let escaped = serde_json::to_string(value)
                .map(|quoted| quoted.trim_matches('"').to_string())
                .unwrap_or_default();
            [value.clone(), escaped]
        })
        .filter(|value| !value.is_empty())
        .collect();
    ordered.sort_by_key(|value| std::cmp::Reverse(value.len()));
    ordered.dedup();
    let mut message = message;
    for value in ordered {
        message = message.replace(value.as_str(), "[REDACTED]");
    }
    message
}

#[derive(Clone)]
pub struct GiteaClient {
    base_url: Url,
    http: reqwest::Client,
    /// Held so a refusal body echoing it can be scrubbed before the message
    /// becomes an error. The header actually sent is built once in `new` and
    /// marked sensitive; this copy never leaves the crate.
    service_token: Arc<str>,
}

impl fmt::Debug for GiteaClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GiteaClient")
            .field("base_url", &self.base_url)
            .field("http", &"[configured]")
            .field("service_token", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GiteaVersion {
    pub version: String,
}

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("invalid Gitea base URL: {0}")]
    InvalidBaseUrl(#[from] url::ParseError),
    #[error("invalid service token")]
    InvalidToken,
    #[error("invalid token-lifecycle credentials")]
    InvalidTokenCredentials,
    #[error("invalid access-token input: {0}")]
    InvalidAccessTokenInput(&'static str),
    #[error("failed to build HTTP client")]
    ClientBuild(#[source] reqwest::Error),
    #[error("Gitea request failed")]
    Transport(#[source] reqwest::Error),
    #[error("Gitea returned HTTP {status}: {message}")]
    Upstream { status: u16, message: String },
    #[error("Gitea returned an invalid response")]
    InvalidResponse(#[source] reqwest::Error),
    #[error("Gitea returned invalid JSON: {0}")]
    InvalidJson(#[source] serde_json::Error),
    #[error("Gitea returned an invalid declared response header")]
    InvalidResponseHeader,
    #[error("Gitea response exceeded the configured bound")]
    ResponseTooLarge,
    #[error("Gitea returned an invalid version")]
    InvalidVersion,
    #[error("generated Gitea operation is invalid")]
    InvalidOperation,
    #[error("Gitea operation is not available through this authentication lane")]
    UnsupportedOperation,
    #[error("Gitea operation arguments are invalid at {0}")]
    InvalidArguments(String),
    #[error("Gitea operation contains an invalid header")]
    InvalidHeader,
    #[error("Gitea upload is invalid or exceeds the configured bound")]
    InvalidUpload,
    #[error(
        "Gitea completed the operation with HTTP {status}, but its response could not be delivered: {source}"
    )]
    ResponseNotDelivered {
        status: u16,
        #[source]
        source: Box<ApiError>,
    },
}

/// What is known about the upstream call when it could not be completed.
///
/// A failed call is not one fact but two: what went wrong, and whether the
/// operation happened anyway. The second is what decides whether retrying is
/// safe, and for a mutation it is the only part that matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallOutcome {
    /// The request was never transmitted. Nothing happened upstream and the
    /// call can be corrected and reissued.
    NotSent,
    /// The request went out and no status came back. Whether Gitea applied it
    /// cannot be determined from here, so reissuing it may repeat a side
    /// effect.
    Unknown,
    /// Gitea answered with this status, so the outcome is settled even though
    /// the response could not be delivered. Reissuing a mutation would perform
    /// it a second time.
    Completed { status: u16 },
}

impl ApiError {
    /// Classify what is known about the upstream call this error ended.
    #[must_use]
    pub fn call_outcome(&self) -> CallOutcome {
        match self {
            // A status came back either way: refused outright, or answered and
            // then lost on the way to the caller.
            Self::Upstream { status, .. } | Self::ResponseNotDelivered { status, .. } => {
                CallOutcome::Completed { status: *status }
            }
            // Only a failure to establish the connection proves nothing was
            // delivered. Everything else — a timeout, a reset, a redirect loop,
            // a body error — can occur after the request was fully written, so
            // the operation may have run. The asymmetry is deliberate: claiming
            // `Unknown` for a call that never left wastes a caller's caution,
            // while claiming `NotSent` for one that ran invites a second
            // deletion. Note that a timeout reports `is_request`, which is why
            // that is not treated as proof of non-delivery.
            Self::Transport(error) => {
                if error.is_connect() {
                    CallOutcome::NotSent
                } else {
                    CallOutcome::Unknown
                }
            }
            // A response was involved, so something was sent. Reached only where
            // the status was not carried into the error; the generated lane
            // wraps these in `ResponseNotDelivered` and reports the status.
            Self::ResponseTooLarge
            | Self::InvalidResponse(_)
            | Self::InvalidJson(_)
            | Self::InvalidResponseHeader
            | Self::InvalidVersion => CallOutcome::Unknown,
            // Configuration, validation, and construction failures all precede
            // the send.
            Self::InvalidBaseUrl(_)
            | Self::InvalidToken
            | Self::InvalidTokenCredentials
            | Self::InvalidAccessTokenInput(_)
            | Self::ClientBuild(_)
            | Self::InvalidOperation
            | Self::UnsupportedOperation
            | Self::InvalidArguments(_)
            | Self::InvalidHeader
            | Self::InvalidUpload => CallOutcome::NotSent,
        }
    }

    /// Record that Gitea answered with `status` before this failure.
    ///
    /// Applied to every failure raised after a status has arrived, whatever its
    /// kind. The outcome of a call is a property of how far it got, not of what
    /// went wrong afterwards: a malformed response-header declaration in the
    /// generated catalog is a defect in this repository, but the deletion it
    /// interrupted still happened. Exempting a class of error here would report
    /// a completed mutation as never sent, which is the failure this
    /// classification exists to prevent.
    ///
    /// What the error *says* is preserved as the source, so the diagnosis is not
    /// traded away for the outcome.
    /// Public because a caller can also learn something only after the status
    /// has arrived — a workflow whose own bound is exceeded by a page Gitea
    /// returned successfully still needs that call recorded as delivered.
    #[must_use]
    pub fn delivered_with(self, status: u16) -> Self {
        match self {
            // Already carries a status; re-wrapping would bury the first one.
            already @ Self::ResponseNotDelivered { .. } => already,
            error => Self::ResponseNotDelivered {
                status,
                source: Box::new(error),
            },
        }
    }
}

impl GiteaClient {
    /// Construct a bounded client for one Gitea instance.
    ///
    /// # Errors
    ///
    /// Returns an error when the base URL, token header, or HTTP client
    /// configuration is invalid.
    pub fn new(base_url: &str, service_token: &str, timeout: Duration) -> Result<Self, ApiError> {
        let base_url = normalize_base_url(base_url)?;
        let mut headers = HeaderMap::new();
        if !service_token.is_empty() {
            let mut token = HeaderValue::from_str(&format!("token {service_token}"))
                .map_err(|_| ApiError::InvalidToken)?;
            token.set_sensitive(true);
            headers.insert(AUTHORIZATION, token);
        }
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .map_err(ApiError::ClientBuild)?;
        Ok(Self {
            base_url,
            http,
            service_token: Arc::from(service_token),
        })
    }

    /// Values that must never survive into an error this client produces.
    pub(crate) fn secrets(&self) -> [String; 1] {
        [self.service_token.to_string()]
    }

    #[must_use]
    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// Return the upstream Gitea version.
    ///
    /// # Errors
    ///
    /// Returns a normalized transport, status, or response-decoding error.
    pub async fn version(&self) -> Result<GiteaVersion, ApiError> {
        let url = self.base_url.join("version")?;
        let mut response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(ApiError::Transport)?;
        let status = response.status();
        if !status.is_success() {
            return Err(ApiError::Upstream {
                status: status.as_u16(),
                message: read_error_message(response, &self.secrets()).await,
            });
        }
        // The status arrived, so everything below is a completed call. Left bare
        // these classify as `Unknown` and tell a caller no response came back,
        // which is plainly false once a 2xx has been read.
        let delivered = |error: ApiError| error.delivered_with(status.as_u16());
        if response
            .content_length()
            .is_some_and(|length| length > MAX_VERSION_RESPONSE_BYTES as u64)
        {
            return Err(delivered(ApiError::ResponseTooLarge));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| delivered(ApiError::InvalidResponse(error)))?
        {
            if body.len().saturating_add(chunk.len()) > MAX_VERSION_RESPONSE_BYTES {
                return Err(delivered(ApiError::ResponseTooLarge));
            }
            body.extend_from_slice(&chunk);
        }
        let version: GiteaVersion = serde_json::from_slice(&body)
            .map_err(|error| delivered(ApiError::InvalidJson(error)))?;
        if version.version.is_empty() || version.version.chars().count() > MAX_VERSION_CHARACTERS {
            return Err(delivered(ApiError::InvalidVersion));
        }
        Ok(version)
    }
}

fn normalize_base_url(raw: &str) -> Result<Url, ApiError> {
    let mut url = Url::parse(raw)?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(ApiError::InvalidBaseUrl(
            url::ParseError::RelativeUrlWithoutBase,
        ));
    }
    url.set_query(None);
    url.set_fragment(None);
    let path = url.path().trim_end_matches('/');
    url.set_path(&format!("{path}/{API_PREFIX}"));
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn normalizes_instance_url_to_api_root() {
        let client = GiteaClient::new("https://gitea.example.test/", "", Duration::from_secs(1))
            .expect("client");
        assert_eq!(
            client.base_url().as_str(),
            "https://gitea.example.test/api/v1/"
        );
    }

    #[test]
    fn rejects_non_http_base_url() {
        let error = GiteaClient::new("file:///tmp/gitea", "", Duration::from_secs(1))
            .expect_err("non-HTTP URL must fail");
        assert!(matches!(error, ApiError::InvalidBaseUrl(_)));
    }

    #[tokio::test]
    async fn a_version_body_that_cannot_be_decoded_is_not_an_unanswered_request() {
        // A 200 arrived, so the call is settled. Left bare these classified as
        // Unknown and told the caller no response came back.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("listener address");
        let upstream = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request");
            let mut request = vec![0_u8; 8_192];
            let _length = socket.read(&mut request).await.expect("read request");
            let body = "not json at all";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });
        let client = GiteaClient::new(&format!("http://{address}"), "t", Duration::from_secs(1))
            .expect("client");

        let Err(error) = client.version().await else {
            panic!("an undecodable version body is an error")
        };
        assert_eq!(
            error.call_outcome(),
            CallOutcome::Completed { status: 200 },
            "the 200 was read before the body failed: {error:?}"
        );
        upstream.await.expect("upstream task");
    }

    #[test]
    fn client_diagnostics_never_render_the_service_token() {
        let client = GiteaClient::new(
            "https://gitea.example.test/",
            "visible-only-if-debug-is-unsafe",
            Duration::from_secs(1),
        )
        .expect("client");

        assert!(!format!("{client:?}").contains("visible-only-if-debug-is-unsafe"));
    }
}
