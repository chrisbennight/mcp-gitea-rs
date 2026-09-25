use std::{fmt, sync::Arc, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use url::Url;

use crate::{ApiError, normalize_base_url};

const MAX_CREDENTIAL_CHARACTERS: usize = 4_096;
const MAX_NAME_CHARACTERS: usize = 255;
const MAX_SCOPE_CHARACTERS: usize = 64;
const MAX_TOKEN_CHARACTERS: usize = 512;
const MAX_TOKEN_RESULTS: u32 = 100;
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const TOKEN_SCOPE_DOMAINS: [&str; 9] = [
    "activitypub",
    "admin",
    "issue",
    "misc",
    "notification",
    "organization",
    "package",
    "repository",
    "user",
];

#[derive(Clone)]
pub struct TokenLifecycleClient {
    base_url: Url,
    http: reqwest::Client,
    username: Arc<str>,
    password: Arc<str>,
}

impl fmt::Debug for TokenLifecycleClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenLifecycleClient")
            .field("base_url", &self.base_url)
            .field("username", &"[configured]")
            .field("password", &"[REDACTED]")
            .field("http", &"[configured]")
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CreateAccessToken {
    pub name: String,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessTokenSelector {
    Id(i64),
    Name(String),
}

#[derive(Deserialize, Serialize)]
pub struct AccessTokenCreated {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(rename(deserialize = "sha1"))]
    pub token: String,
    #[serde(default)]
    pub token_last_eight: String,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AccessTokenMetadata {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub token_last_eight: String,
    pub created_at: Option<String>,
    pub last_used_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AccessTokenPage {
    pub tokens: Vec<AccessTokenMetadata>,
    pub pagination: crate::Pagination,
}

impl TokenLifecycleClient {
    /// Construct the Basic Auth client used only for access-token lifecycle.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid base URL, empty or unbounded
    /// credentials, or an invalid HTTP client configuration.
    pub fn new(
        base_url: &str,
        username: &str,
        password: &str,
        timeout: Duration,
    ) -> Result<Self, ApiError> {
        validate_credential(username, MAX_NAME_CHARACTERS)?;
        if username.contains(':') {
            return Err(ApiError::InvalidTokenCredentials);
        }
        validate_credential(password, MAX_CREDENTIAL_CHARACTERS)?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .map_err(ApiError::ClientBuild)?;
        Ok(Self {
            base_url: normalize_base_url(base_url)?,
            http,
            username: Arc::from(username),
            password: Arc::from(password),
        })
    }

    /// Create a scoped access token for the configured Basic Auth identity.
    ///
    /// # Errors
    ///
    /// Returns a validation, transport, upstream status, size, or decoding
    /// error. A transport failure is ambiguous and is never retried.
    pub async fn create(
        &self,
        request: &CreateAccessToken,
    ) -> Result<AccessTokenCreated, ApiError> {
        validate_create_access_token(request)?;
        let response = self
            .authenticated(self.http.post(self.tokens_url()?))
            .json(request)
            .send()
            .await
            .map_err(ApiError::Transport)?;
        if response.status() != StatusCode::CREATED {
            return Err(self.refusal(response).await);
        }
        // The status was accepted before the body was read, so a size or decode
        // failure below is a completed call, not an unknown one.
        let status = response.status().as_u16();
        let token: AccessTokenCreated = read_json(response)
            .await
            .map_err(|error| error.delivered_with(status))?;
        // A 201 whose body does not describe a usable token is still a token
        // that exists. Reporting the caller's input as invalid would say the
        // request never ran, and for this lane that means a live credential
        // nobody knows about.
        validate_created(&token).map_err(|error| error.delivered_with(status))?;
        Ok(token)
    }

    /// List bounded access-token metadata for the configured identity.
    ///
    /// # Errors
    ///
    /// Returns a validation, transport, upstream status, size, or decoding
    /// error.
    pub async fn list(
        &self,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<Vec<AccessTokenMetadata>, ApiError> {
        Ok(self.list_page(page, limit).await?.tokens)
    }

    /// List token metadata with safe continuation information.
    ///
    /// # Errors
    ///
    /// Returns the same bounded transport and validation errors as [`Self::list`].
    pub async fn list_page(
        &self,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<AccessTokenPage, ApiError> {
        validate_pagination(page, limit)?;
        let mut url = self.tokens_url()?;
        {
            let mut query = url.query_pairs_mut();
            if let Some(page) = page {
                query.append_pair("page", &page.to_string());
            }
            if let Some(limit) = limit {
                query.append_pair("limit", &limit.to_string());
            }
        }
        let response = self
            .authenticated(self.http.get(url))
            .send()
            .await
            .map_err(ApiError::Transport)?;
        if response.status() != StatusCode::OK {
            return Err(self.refusal(response).await);
        }
        let status = response.status().as_u16();
        let mut pagination = crate::Pagination::from_headers(response.headers(), response.url());
        let tokens: Vec<AccessTokenMetadata> = read_json(response)
            .await
            .map_err(|error| error.delivered_with(status))?;
        let max_results =
            usize::try_from(MAX_TOKEN_RESULTS).map_err(|_| ApiError::InvalidOperation)?;
        if tokens.len() > max_results || tokens.iter().any(invalid_metadata) {
            return Err(
                ApiError::InvalidAccessTokenInput("Gitea returned invalid token metadata")
                    .delivered_with(status),
            );
        }
        pagination.observe_empty_page(tokens.is_empty());
        Ok(AccessTokenPage { tokens, pagination })
    }

    /// Revoke one access token by an explicit Gitea identifier or name.
    ///
    /// # Errors
    ///
    /// Returns a validation, transport, or upstream status error. A transport
    /// failure is ambiguous and is never retried.
    pub async fn revoke(&self, selector: &AccessTokenSelector) -> Result<(), ApiError> {
        let token = match selector {
            AccessTokenSelector::Id(id) if *id > 0 => id.to_string(),
            AccessTokenSelector::Id(_) => {
                return Err(ApiError::InvalidAccessTokenInput(
                    "token identifier must be positive",
                ));
            }
            AccessTokenSelector::Name(name) => {
                validate_token_name(name)?;
                name.clone()
            }
        };
        let mut url = self.tokens_url()?;
        url.path_segments_mut()
            .map_err(|()| ApiError::InvalidOperation)?
            .push(&token);
        let response = self
            .authenticated(self.http.delete(url))
            .send()
            .await
            .map_err(ApiError::Transport)?;
        if response.status() != StatusCode::NO_CONTENT {
            return Err(self.refusal(response).await);
        }
        Ok(())
    }

    fn authenticated(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request.basic_auth(self.username.as_ref(), Some(self.password.as_ref()))
    }

    /// Turn a non-success response into a refusal that says why.
    ///
    /// The reason is what a caller needs to correct the request — a scope Gitea
    /// does not recognise, a name already taken — and without it the only
    /// available move is to try the same call again.
    ///
    /// The configured Basic Auth credentials are redacted from the reason
    /// before it leaves this function. Gitea is not expected to echo them, but
    /// this lane is the one place where they exist, and an error message is a
    /// durable artifact: it reaches logs, transcripts, and the model.
    /// Redaction happens inside the read, before truncation. Scrubbing a
    /// message that has already been cut can miss a credential straddling the
    /// boundary, because only a complete occurrence matches.
    async fn refusal(&self, response: reqwest::Response) -> ApiError {
        let status = response.status().as_u16();
        let message = crate::read_error_message(response, &self.secrets()).await;
        ApiError::Upstream { status, message }
    }

    /// Values that must never survive into an error this client produces.
    ///
    /// Every representation that goes on the wire, not just the ones a reader
    /// would name. The Basic blob is what an echoed `Authorization` header
    /// contains, and base64 is reversible. The percent-encoded username is what
    /// an echoed request path contains, and `validate_credential` admits
    /// characters — `%`, `/` — whose encoded form differs from the raw value,
    /// so redacting only the raw one would leave the path copy intact.
    fn secrets(&self) -> [String; 4] {
        [
            self.username.to_string(),
            self.password.to_string(),
            STANDARD.encode(format!("{}:{}", self.username, self.password)),
            self.path_encoded_username(),
        ]
    }

    /// The username exactly as `tokens_url` writes it into a path segment.
    ///
    /// Derived through the same URL machinery rather than a hand-rolled
    /// encoder, so the two cannot disagree about what a character becomes.
    fn path_encoded_username(&self) -> String {
        let mut url = self.base_url.clone();
        let Ok(mut segments) = url.path_segments_mut() else {
            return self.username.to_string();
        };
        segments.clear().push(self.username.as_ref());
        drop(segments);
        url.path().trim_start_matches('/').to_string()
    }

    fn tokens_url(&self) -> Result<Url, ApiError> {
        let mut url = self.base_url.clone();
        let mut segments = url
            .path_segments_mut()
            .map_err(|()| ApiError::InvalidOperation)?;
        segments
            .pop_if_empty()
            .extend(["users", self.username.as_ref(), "tokens"]);
        drop(segments);
        Ok(url)
    }
}

fn validate_credential(value: &str, max_characters: usize) -> Result<(), ApiError> {
    if value.is_empty()
        || value.chars().count() > max_characters
        || value.chars().any(char::is_control)
    {
        return Err(ApiError::InvalidTokenCredentials);
    }
    Ok(())
}

/// Validate an access-token request without performing an external mutation.
///
/// # Errors
///
/// Returns [`ApiError::InvalidAccessTokenInput`] when the token name or scopes
/// do not satisfy Gitea's supported, bounded token contract.
pub fn validate_create_access_token(request: &CreateAccessToken) -> Result<(), ApiError> {
    validate_token_name(&request.name)?;
    if request.scopes.is_empty() || request.scopes.len() > TOKEN_SCOPE_DOMAINS.len() {
        return Err(ApiError::InvalidAccessTokenInput(
            "scopes must contain a bounded non-empty set",
        ));
    }
    if request.scopes.iter().any(|scope| {
        scope.is_empty() || scope.chars().count() > MAX_SCOPE_CHARACTERS || !valid_scope(scope)
    }) {
        return Err(ApiError::InvalidAccessTokenInput(
            "scope is not supported by Gitea",
        ));
    }
    if request.scopes.iter().any(|scope| scope == "all") && request.scopes.len() != 1 {
        return Err(ApiError::InvalidAccessTokenInput(
            "the all scope cannot be combined with other scopes",
        ));
    }
    let mut scopes = request.scopes.iter().collect::<Vec<_>>();
    scopes.sort_unstable();
    if scopes.windows(2).any(|window| window[0] == window[1]) {
        return Err(ApiError::InvalidAccessTokenInput(
            "scopes must not contain duplicates",
        ));
    }
    Ok(())
}

fn validate_token_name(name: &str) -> Result<(), ApiError> {
    validate_bounded_text(name, MAX_NAME_CHARACTERS, "token name is invalid")?;
    let unsigned = name
        .strip_prefix('+')
        .or_else(|| name.strip_prefix('-'))
        .unwrap_or(name);
    if !unsigned.is_empty() && unsigned.bytes().all(|character| character.is_ascii_digit()) {
        return Err(ApiError::InvalidAccessTokenInput(
            "token name must not be a signed or unsigned decimal identifier",
        ));
    }
    Ok(())
}

fn valid_scope(scope: &str) -> bool {
    if scope == "all" {
        return true;
    }
    let Some((access, domain)) = scope.split_once(':') else {
        return false;
    };
    matches!(access, "read" | "write") && TOKEN_SCOPE_DOMAINS.contains(&domain)
}

fn validate_created(token: &AccessTokenCreated) -> Result<(), ApiError> {
    validate_bounded_text(&token.name, MAX_NAME_CHARACTERS, "invalid token response")?;
    validate_bounded_text(&token.token, MAX_TOKEN_CHARACTERS, "invalid token response")?;
    if token.scopes.len() > TOKEN_SCOPE_DOMAINS.len()
        || token
            .scopes
            .iter()
            .any(|scope| scope.chars().count() > MAX_SCOPE_CHARACTERS || !valid_scope(scope))
        || token.token_last_eight.chars().count() > 8
    {
        return Err(ApiError::InvalidAccessTokenInput(
            "Gitea returned an invalid token",
        ));
    }
    Ok(())
}

fn invalid_metadata(token: &AccessTokenMetadata) -> bool {
    token.name.is_empty()
        || token.name.chars().count() > MAX_NAME_CHARACTERS
        || token.scopes.len() > TOKEN_SCOPE_DOMAINS.len()
        || token
            .scopes
            .iter()
            .any(|scope| scope.chars().count() > MAX_SCOPE_CHARACTERS || !valid_scope(scope))
        || token.token_last_eight.chars().count() > 8
}

fn validate_pagination(page: Option<u32>, limit: Option<u32>) -> Result<(), ApiError> {
    if page == Some(0) || limit.is_some_and(|value| !(1..=MAX_TOKEN_RESULTS).contains(&value)) {
        return Err(ApiError::InvalidAccessTokenInput(
            "pagination is outside the supported bounds",
        ));
    }
    Ok(())
}

fn validate_bounded_text(
    value: &str,
    max_characters: usize,
    message: &'static str,
) -> Result<(), ApiError> {
    if value.is_empty()
        || value.chars().count() > max_characters
        || value.chars().any(char::is_control)
    {
        return Err(ApiError::InvalidAccessTokenInput(message));
    }
    Ok(())
}

async fn read_json<T: DeserializeOwned>(mut response: reqwest::Response) -> Result<T, ApiError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(ApiError::ResponseTooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(ApiError::InvalidResponse)? {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(ApiError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(ApiError::InvalidJson)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn scopes_are_explicit_bounded_and_unique() {
        assert!(
            validate_create_access_token(&CreateAccessToken {
                name: "agent".to_string(),
                scopes: vec!["write:repository".to_string(), "read:user".to_string()],
            })
            .is_ok()
        );
        for scopes in [
            Vec::new(),
            vec!["all".to_string(), "read:user".to_string()],
            vec![
                "write:repository".to_string(),
                "write:repository".to_string(),
            ],
            vec!["write:unknown".to_string()],
        ] {
            assert!(
                validate_create_access_token(&CreateAccessToken {
                    name: "agent".to_string(),
                    scopes,
                })
                .is_err()
            );
        }
    }

    #[test]
    fn token_names_cannot_alias_signed_decimal_identifiers() {
        for name in ["7", "0007", "+7", "+0007", "-7", "-0007"] {
            assert!(
                validate_token_name(name).is_err(),
                "{name} aliases Gitea's numeric token selector"
            );
        }
        for name in ["agent7", "7agent", "+agent", "-", "agent-name"] {
            assert!(
                validate_token_name(name).is_ok(),
                "{name} is not a decimal identifier"
            );
        }
    }

    #[test]
    fn diagnostics_never_render_basic_credentials() {
        let client = TokenLifecycleClient::new(
            "https://gitea.example.test",
            "visible-user",
            "visible-password",
            Duration::from_secs(1),
        )
        .expect("client");
        let diagnostic = format!("{client:?}");
        assert!(!diagnostic.contains("visible-user"));
        assert!(!diagnostic.contains("visible-password"));
        assert!(
            TokenLifecycleClient::new(
                "https://gitea.example.test",
                "invalid:basic-user",
                "password",
                Duration::from_secs(1),
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn lifecycle_encodes_basic_auth_paths_queries_and_secret_result() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("listener address");
        let expected_auth = format!(
            "authorization: Basic {}",
            STANDARD.encode("bot user:password")
        );
        let upstream = tokio::spawn(async move {
            for (request_line, status, body) in [
                (
                    "POST /api/v1/users/bot%20user/tokens HTTP/1.1\r\n",
                    "201 Created",
                    r#"{"id":7,"name":"agent","scopes":["write:repository"],"sha1":"one-time-token","token_last_eight":"me-token","created_at":"2026-07-25T00:00:00Z"}"#,
                ),
                (
                    "GET /api/v1/users/bot%20user/tokens?page=2&limit=25 HTTP/1.1\r\n",
                    "200 OK",
                    r#"[{"id":7,"name":"agent","scopes":["write:repository"],"token_last_eight":"me-token","created_at":"2026-07-25T00:00:00Z","last_used_at":null}]"#,
                ),
                (
                    "DELETE /api/v1/users/bot%20user/tokens/agent%2Fone HTTP/1.1\r\n",
                    "204 No Content",
                    "",
                ),
            ] {
                let (mut socket, _) = listener.accept().await.expect("request");
                let mut request = vec![0_u8; 8_192];
                let length = socket.read(&mut request).await.expect("read request");
                let request = String::from_utf8_lossy(&request[..length]);
                assert!(request.starts_with(request_line), "{request}");
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains(&expected_auth.to_ascii_lowercase()),
                    "{request}"
                );
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }
        });
        let client = TokenLifecycleClient::new(
            &format!("http://{address}"),
            "bot user",
            "password",
            Duration::from_secs(1),
        )
        .expect("client");

        let created = client
            .create(&CreateAccessToken {
                name: "agent".to_string(),
                scopes: vec!["write:repository".to_string()],
            })
            .await
            .expect("create");
        assert_eq!(created.id, 7);
        assert_eq!(created.token, "one-time-token");

        let listed = client.list(Some(2), Some(25)).await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "agent");

        client
            .revoke(&AccessTokenSelector::Name("agent/one".to_string()))
            .await
            .expect("revoke");
        upstream.await.expect("upstream task");
    }

    #[tokio::test]
    async fn transport_errors_do_not_disclose_the_configured_username() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("listener address");
        let upstream = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("request");
            drop(socket);
        });
        let client = TokenLifecycleClient::new(
            &format!("http://{address}"),
            "sensitive-automation-user",
            "sensitive-password",
            Duration::from_secs(1),
        )
        .expect("client");

        let error = client.list(None, None).await.expect_err("closed response");
        let public_error = error.to_string();
        assert_eq!(public_error, "Gitea request failed");
        assert!(!public_error.contains("sensitive-automation-user"));
        assert!(!public_error.contains("sensitive-password"));
        upstream.await.expect("upstream task");
    }

    async fn refusing_upstream(
        status_line: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("listener address");
        let upstream = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request");
            let mut request = vec![0_u8; 8_192];
            let _length = socket.read(&mut request).await.expect("read request");
            let response = format!(
                "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });
        (format!("http://{address}"), upstream)
    }

    #[tokio::test]
    async fn a_refusal_carries_the_reason_gitea_gave() {
        // Without this the caller sees only "HTTP 422" and has no way to tell a
        // name collision from an unrecognised scope, which is the difference
        // between fixing the call and reissuing it unchanged.
        let (base_url, upstream) = refusing_upstream(
            "422 Unprocessable Entity",
            r#"{"message":"access token name has already been used"}"#,
        )
        .await;
        let client = TokenLifecycleClient::new(
            &base_url,
            "token-user",
            "token-password",
            Duration::from_secs(1),
        )
        .expect("client");

        let Err(error) = client
            .create(&CreateAccessToken {
                name: "agent".to_string(),
                scopes: vec!["read:user".to_string()],
            })
            .await
        else {
            panic!("a 422 is a refusal")
        };
        assert!(
            matches!(&error, ApiError::Upstream { status: 422, message }
                if message == "access token name has already been used"),
            "the reason survives: {error:?}"
        );
        assert_eq!(
            error.call_outcome(),
            crate::CallOutcome::Completed { status: 422 }
        );
        upstream.await.expect("upstream task");
    }

    #[tokio::test]
    async fn a_refusal_that_echoes_the_credentials_redacts_them() {
        // This lane is the only place the Basic Auth credentials exist, and an
        // error message is durable: it reaches logs, transcripts, and the model.
        // Gitea is not expected to echo them; the guarantee must not rest on
        // that.
        let (base_url, upstream) = refusing_upstream(
            "401 Unauthorized",
            r#"{"message":"user sensitive-automation-user with password hunter2-not-real failed"}"#,
        )
        .await;
        let client = TokenLifecycleClient::new(
            &base_url,
            "sensitive-automation-user",
            "hunter2-not-real",
            Duration::from_secs(1),
        )
        .expect("client");

        let Err(error) = client.revoke(&AccessTokenSelector::Id(7)).await else {
            panic!("a 401 is a refusal")
        };
        let rendered = error.to_string();
        assert!(
            !rendered.contains("sensitive-automation-user")
                && !rendered.contains("hunter2-not-real"),
            "credentials must not survive into the error: {rendered}"
        );
        assert!(
            rendered.contains("[REDACTED]"),
            "redaction is visible: {rendered}"
        );
        assert!(
            rendered.contains("401"),
            "the status still reaches the caller: {rendered}"
        );
        upstream.await.expect("upstream task");
    }

    #[tokio::test]
    async fn a_refusal_echoing_the_authorization_header_redacts_the_basic_value() {
        // What goes on the wire is base64("user:password"), not the raw pair.
        // Redacting only the raw values leaves that blob intact, and base64 is
        // reversible — leaking it leaks the password.
        let username = "token-user";
        let password = "hunter2-not-real";
        let encoded = STANDARD.encode(format!("{username}:{password}"));
        let body = leak(format!(
            r#"{{"message":"rejected credential Basic {encoded}"}}"#
        ));

        let (base_url, upstream) = refusing_upstream("401 Unauthorized", body).await;
        let client =
            TokenLifecycleClient::new(&base_url, username, password, Duration::from_secs(1))
                .expect("client");

        let Err(error) = client.list(None, None).await else {
            panic!("a 401 is a refusal")
        };
        let rendered = error.to_string();
        assert!(
            !rendered.contains(&encoded),
            "the encoded Basic credential must not survive: {rendered}"
        );
        assert!(
            rendered.contains("[REDACTED]"),
            "redaction is visible: {rendered}"
        );
        upstream.await.expect("upstream task");
    }

    #[tokio::test]
    async fn a_refusal_echoing_the_request_path_redacts_the_encoded_username() {
        // validate_credential admits '%' and '/', whose percent-encoded form in
        // a path segment differs from the raw username. Redacting only the raw
        // value would leave the path copy intact.
        //
        // Driven through `list`. An earlier version called `secrets` and
        // `redact` directly, so it stayed green even if the production refusal
        // path stopped passing the encoded form to the reducer.
        let username = "svc/automation%40acme";
        let password = "hunter2-not-real";
        let encoded = TokenLifecycleClient::new(
            "http://placeholder.invalid",
            username,
            password,
            Duration::from_secs(1),
        )
        .expect("client")
        .path_encoded_username();
        assert_ne!(
            encoded, username,
            "the fixture is only meaningful if the two forms differ"
        );
        let body = leak(format!(r#"{{"message":"no such user {encoded}"}}"#));

        let (base_url, upstream) = refusing_upstream("404 Not Found", body).await;
        let client =
            TokenLifecycleClient::new(&base_url, username, password, Duration::from_secs(1))
                .expect("client");

        let Err(error) = client.list(None, None).await else {
            panic!("a 404 is a refusal")
        };
        let rendered = error.to_string();
        assert!(
            !rendered.contains(&encoded),
            "the wire form of the username must not survive: {rendered}"
        );
        assert!(
            rendered.contains("[REDACTED]"),
            "redaction is visible: {rendered}"
        );
        upstream.await.expect("upstream task");
    }

    #[tokio::test]
    async fn a_created_token_that_fails_validation_is_not_reported_as_never_sent() {
        // The worst case this whole contract exists for. Gitea answered 201, so
        // a token may be live; if its body does not describe a usable one, the
        // caller must still learn the call happened. Reporting invalid input
        // would say nothing ran and leave a credential nobody knows about.
        let (base_url, upstream) = refusing_upstream_typed(
            "201 Created",
            "application/json",
            r#"{"id":7,"name":"agent","sha1":"","scopes":["read:user"]}"#,
        )
        .await;
        let client = TokenLifecycleClient::new(
            &base_url,
            "token-user",
            "token-password",
            Duration::from_secs(1),
        )
        .expect("client");

        let Err(error) = client
            .create(&CreateAccessToken {
                name: "agent".to_string(),
                scopes: vec!["read:user".to_string()],
            })
            .await
        else {
            panic!("an empty token value is not usable")
        };
        assert_eq!(
            error.call_outcome(),
            crate::CallOutcome::Completed { status: 201 },
            "201 was delivered, so the call is settled: {error:?}"
        );
        upstream.await.expect("upstream task");
    }

    #[tokio::test]
    async fn an_uppercase_json_media_type_is_still_decoded_as_json() {
        // HTTP media types are case-insensitive. Treating `Application/JSON` as
        // opaque text made the whole serialized document the message, which both
        // buries the reason and exposes the escaped forms inside it.
        let (base_url, upstream) = refusing_upstream_typed(
            "422 Unprocessable Entity",
            "Application/JSON",
            r#"{"message":"scope is not recognised"}"#,
        )
        .await;
        let client = TokenLifecycleClient::new(
            &base_url,
            "token-user",
            "token-password",
            Duration::from_secs(1),
        )
        .expect("client");

        let Err(error) = client.list(None, None).await else {
            panic!("a 422 is a refusal")
        };
        assert!(
            matches!(&error, ApiError::Upstream { status: 422, message }
                if message == "scope is not recognised"),
            "the decoded message, not the raw document: {error:?}"
        );
        upstream.await.expect("upstream task");
    }

    #[tokio::test]
    async fn a_credential_containing_json_metacharacters_is_redacted_when_escaped() {
        // A password with a quote or a backslash appears escaped inside a body
        // reproduced verbatim, so the raw form does not match there.
        let password = r#"pa"ss\word"#;
        let escaped = serde_json::to_string(password).expect("json");
        let body = leak(format!(
            r#"{{"other":"rejected {}"}}"#,
            escaped.trim_matches('"')
        ));
        // text/plain, so the body itself becomes the reason — that is the branch
        // where an escaped credential appears. Under application/json the
        // extractor would look for a `message` key, find none, and fall back to
        // the generic sentence, which would let this test pass either way.
        let (base_url, upstream) =
            refusing_upstream_typed("401 Unauthorized", "text/plain", body).await;
        let client =
            TokenLifecycleClient::new(&base_url, "token-user", password, Duration::from_secs(1))
                .expect("client");

        let Err(error) = client.list(None, None).await else {
            panic!("a 401 is a refusal")
        };
        let rendered = error.to_string();
        assert!(
            !rendered.contains(escaped.trim_matches('"')),
            "the escaped credential must not survive: {rendered}"
        );
        upstream.await.expect("upstream task");
    }

    async fn refusing_upstream_typed(
        status_line: &'static str,
        content_type: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("listener address");
        let upstream = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request");
            let mut request = vec![0_u8; 8_192];
            let _length = socket.read(&mut request).await.expect("read request");
            let response = format!(
                "HTTP/1.1 {status_line}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });
        (format!("http://{address}"), upstream)
    }

    #[tokio::test]
    async fn a_refusal_whose_body_read_fails_midway_yields_no_partial_reason() {
        // A chunk error used to end the loop like a clean EOF, so whatever had
        // arrived was normalized. The cut can fall inside a credential, and
        // redaction matches only complete occurrences.
        //
        // The body is text/plain deliberately. A JSON body cut mid-credential is
        // invalid JSON, so extraction already falls back to the generic
        // sentence; the exposure is reachable only where the raw bytes become
        // the message.
        let password = "hunter2-not-real-and-quite-long-indeed";
        let leaked_prefix: String = password.chars().take(20).collect();
        // The delivered bytes stop *inside* the credential, which is what a
        // connection dying mid-body produces. Padding after it makes the prefix
        // land in a chunk the client actually receives before the read fails.
        let body = leak(format!(
            "rejected credential {leaked_prefix}{}",
            "x".repeat(8_192)
        ));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("listener address");
        let upstream = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request");
            let mut request = vec![0_u8; 8_192];
            let _length = socket.read(&mut request).await.expect("read request");
            // Promise far more than is sent, then hang up mid-body.
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\ncontent-type: text/plain\r\ncontent-length: {}\r\n\r\n{body}",
                body.len() + 4_096
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
            drop(socket);
        });
        let client = TokenLifecycleClient::new(
            &format!("http://{address}"),
            "token-user",
            password,
            Duration::from_secs(2),
        )
        .expect("client");

        let Err(error) = client.list(None, None).await else {
            panic!("a 401 is a refusal")
        };
        let rendered = error.to_string();
        assert!(
            !rendered.contains(&leaked_prefix),
            "no credential run from a partial body: {rendered}"
        );
        assert!(
            rendered.contains("401"),
            "the status still reaches the caller: {rendered}"
        );
        upstream.await.expect("upstream task");
    }

    #[tokio::test]
    async fn an_accepted_status_survives_a_body_that_cannot_be_decoded() {
        // Drives `create` for real. An earlier version of this test built the
        // error by hand and asserted its classification, which proved only that
        // `call_outcome` works — reverting the `delivered_with` wrapping in the
        // production path left it green.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("listener address");
        let upstream = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request");
            let mut request = vec![0_u8; 8_192];
            let _length = socket.read(&mut request).await.expect("read request");
            // 201 accepted, then a declared length past the read cap.
            let response = format!(
                "HTTP/1.1 201 Created\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                MAX_RESPONSE_BYTES + 1
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });
        let client = TokenLifecycleClient::new(
            &format!("http://{address}"),
            "token-user",
            "token-password",
            Duration::from_secs(1),
        )
        .expect("client");

        let Err(error) = client
            .create(&CreateAccessToken {
                name: "agent".to_string(),
                scopes: vec!["read:user".to_string()],
            })
            .await
        else {
            panic!("an undecodable body is an error")
        };
        assert_eq!(
            error.call_outcome(),
            crate::CallOutcome::Completed { status: 201 },
            "201 was accepted before the body was read, so a token may exist: {error:?}"
        );
        upstream.await.expect("upstream task");
    }

    fn leak(value: String) -> &'static str {
        Box::leak(value.into_boxed_str())
    }

    #[tokio::test]
    async fn a_credential_crossing_the_truncation_boundary_is_still_redacted() {
        // Passwords are accepted up to MAX_CREDENTIAL_CHARACTERS, which is
        // larger than the 2 KiB message cap. Redacting after truncation would
        // leave a straddling credential unmatched, and its surviving prefix
        // would reach the error: redaction replaces complete occurrences only.
        const PASSWORD_LENGTH: usize = 3_000;
        let password = "p".repeat(PASSWORD_LENGTH);
        // Push the credential across the cap: 1 KiB of filler, then the value.
        let body = format!(
            r#"{{"message":"{}{}"}}"#,
            "x".repeat(1_024),
            "p".repeat(PASSWORD_LENGTH)
        );
        let leaked_prefix = "p".repeat(64);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("listener address");
        let upstream = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request");
            let mut request = vec![0_u8; 8_192];
            let _length = socket.read(&mut request).await.expect("read request");
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });
        let client = TokenLifecycleClient::new(
            &format!("http://{address}"),
            "token-user",
            &password,
            Duration::from_secs(1),
        )
        .expect("client");

        let Err(error) = client.list(None, None).await else {
            panic!("a 401 is a refusal")
        };
        let rendered = error.to_string();
        assert!(
            !rendered.contains(&leaked_prefix),
            "no run of the password survives truncation: {} characters rendered",
            rendered.chars().count()
        );
        upstream.await.expect("upstream task");
    }

    #[tokio::test]
    async fn a_refusal_with_no_readable_body_still_reports_its_status() {
        // Losing the reason must not also lose the status; that would put the
        // lane back where it started.
        let (base_url, upstream) = refusing_upstream("500 Internal Server Error", "").await;
        let client = TokenLifecycleClient::new(
            &base_url,
            "token-user",
            "token-password",
            Duration::from_secs(1),
        )
        .expect("client");

        let Err(error) = client.list(None, None).await else {
            panic!("a 500 is a refusal")
        };
        assert!(
            matches!(&error, ApiError::Upstream { status: 500, message }
                if message == "Gitea rejected the operation"),
            "a generic reason, not a missing one: {error:?}"
        );
        upstream.await.expect("upstream task");
    }

    #[tokio::test]
    async fn preserving_redirects_do_not_resubmit_token_creation() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("listener address");
        let upstream = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request");
            let mut request = vec![0_u8; 8_192];
            let _length = socket.read(&mut request).await.expect("read request");
            socket
                .write_all(
                    b"HTTP/1.1 307 Temporary Redirect\r\nlocation: /api/v1/redirected\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .expect("write response");
        });
        let client = TokenLifecycleClient::new(
            &format!("http://{address}"),
            "token-user",
            "token-password",
            Duration::from_secs(1),
        )
        .expect("client");

        let result = client
            .create(&CreateAccessToken {
                name: "agent".to_string(),
                scopes: vec!["read:user".to_string()],
            })
            .await;
        let Err(error) = result else {
            panic!("redirect must remain an upstream response");
        };

        assert!(matches!(error, ApiError::Upstream { status: 307, .. }));
        upstream.await.expect("single upstream request");
    }
}
