use std::{sync::Arc, time::Duration};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::{Bytes, BytesMut};
use gitea_api::{GiteaClient, TokenLifecycleClient};
use gitea_mcp::{
    GiteaMcp,
    files::{FileError, FilePlane, MAX_SECRET_BYTES, TRANSFER_CREDENTIAL_HEADER},
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use serde::Serialize;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use zeroize::{Zeroize, Zeroizing};

use crate::{auth::BearerLayer, config::Settings};

#[derive(Clone, Serialize)]
struct Health {
    status: &'static str,
    tool_count: usize,
}

#[derive(Clone)]
struct UploadState {
    files: Arc<FilePlane>,
    body_slots: Arc<Semaphore>,
    body_timeout: Duration,
}

#[derive(Clone)]
struct IngressState {
    slots: Arc<Semaphore>,
    max_bytes: usize,
    body_timeout: Duration,
    allowed_origins: Vec<String>,
}

enum ZeroizingUploadBody {
    Owned(BytesMut),
    Copied(Zeroizing<Vec<u8>>),
}

impl From<Bytes> for ZeroizingUploadBody {
    fn from(bytes: Bytes) -> Self {
        match bytes.try_into_mut() {
            Ok(bytes) => Self::Owned(bytes),
            // Shared transport storage cannot be mutated without violating its
            // other owners. Keep no clone of it and put the application-owned
            // copy under zeroization for validation and staging.
            Err(bytes) => Self::Copied(Zeroizing::new(bytes.to_vec())),
        }
    }
}

impl AsRef<[u8]> for ZeroizingUploadBody {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
            Self::Copied(bytes) => bytes,
        }
    }
}

impl Drop for ZeroizingUploadBody {
    fn drop(&mut self) {
        if let Self::Owned(bytes) = self {
            bytes.as_mut().zeroize();
        }
    }
}

pub fn router(
    settings: &Settings,
    client: Arc<GiteaClient>,
    token_client: impl Into<Option<Arc<TokenLifecycleClient>>>,
    files: Option<Arc<FilePlane>>,
    cancellation: &CancellationToken,
) -> Router {
    let mcp = GiteaMcp::new(client, token_client)
        .with_files(files.clone())
        .with_execution_limit(settings.max_concurrent_requests);
    let service = StreamableHttpService::new(
        move || Ok(mcp.new_session()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_cancellation_token(cancellation.child_token())
            .with_allowed_hosts(settings.allowed_hosts.clone()),
    );
    let health = Health {
        status: "ok",
        tool_count: GiteaMcp::list_tools_payload().tools.len(),
    };
    let protected = apply_ingress_controls(Router::new().nest_service("/mcp", service), settings);
    let mut router = Router::new()
        .route(
            "/healthz",
            get(move || {
                let health = health.clone();
                async move { (StatusCode::OK, Json(health)) }
            }),
        )
        .merge(protected);
    if let Some(files) = files {
        let upload_state = UploadState {
            files,
            body_slots: Arc::new(Semaphore::new(settings.max_concurrent_requests)),
            body_timeout: settings.timeout,
        };
        router = router.merge(
            Router::new()
                .route("/files/upload/{id}", axum::routing::put(receive_file))
                .route("/files/download/{id}", get(send_download))
                .with_state(upload_state),
        );
    }
    router
}

async fn receive_file(
    State(state): State<UploadState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    body: Body,
) -> Response {
    let credential = headers
        .get(TRANSFER_CREDENTIAL_HEADER)
        .and_then(|value| value.to_str().ok());
    let Some(credential) = credential else {
        return file_refusal(StatusCode::FORBIDDEN, "gitea_file_unauthorized");
    };
    if id.len() > 64 {
        return file_refusal(StatusCode::FORBIDDEN, "gitea_file_unauthorized");
    }
    if let Err(error) = state.files.verify_upload(&id, credential) {
        // This route deliberately has no gateway bearer. Authentication
        // failures are ordinary Internet noise and must not amplify into log
        // storage before the bounded request-body path is entered.
        return file_refusal(StatusCode::FORBIDDEN, error.code());
    }
    let Ok(Ok(_permit)) = tokio::time::timeout(
        state.body_timeout,
        Arc::clone(&state.body_slots).acquire_owned(),
    )
    .await
    else {
        return file_refusal(StatusCode::SERVICE_UNAVAILABLE, "gitea_file_busy");
    };
    let bytes = match tokio::time::timeout(state.body_timeout, to_bytes(body, MAX_SECRET_BYTES + 1))
        .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => {
            return file_refusal(StatusCode::PAYLOAD_TOO_LARGE, "gitea_file_too_large");
        }
        Err(_) => {
            return file_refusal(StatusCode::REQUEST_TIMEOUT, "gitea_file_body_timeout");
        }
    };
    let bytes = ZeroizingUploadBody::from(bytes);
    match state.files.receive(&id, credential, bytes.as_ref()) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            let status = match error {
                FileError::Unauthorized
                | FileError::Expired
                | FileError::Unavailable
                | FileError::MissingResult => StatusCode::FORBIDDEN,
                FileError::TooManyOutstanding => StatusCode::SERVICE_UNAVAILABLE,
                FileError::EntropyUnavailable => StatusCode::INTERNAL_SERVER_ERROR,
                FileError::TooLarge | FileError::SizeMismatch => StatusCode::PAYLOAD_TOO_LARGE,
                FileError::DigestMismatch
                | FileError::UnsupportedDigest
                | FileError::InvalidMetadata
                | FileError::InvalidSecret => StatusCode::BAD_REQUEST,
            };
            tracing::warn!(target: "gitea_server::diagnostics", code = error.code(), "refused a file upload");
            file_refusal(status, error.code())
        }
    }
}

fn file_refusal(status: StatusCode, code: &str) -> Response {
    (status, Json(serde_json::json!({"error": code}))).into_response()
}

async fn send_download(
    State(state): State<UploadState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(credential) = headers
        .get(TRANSFER_CREDENTIAL_HEADER)
        .and_then(|value| value.to_str().ok())
    else {
        return file_refusal(StatusCode::FORBIDDEN, "gitea_file_unauthorized");
    };
    let Ok(permit) = state.body_slots.clone().try_acquire_owned() else {
        return file_refusal(StatusCode::SERVICE_UNAVAILABLE, "gitea_file_busy");
    };
    let resource = match state.files.read_download(&id, credential) {
        Ok(resource) => resource,
        Err(error) => return file_refusal(StatusCode::FORBIDDEN, error.code()),
    };
    let length = resource.body.len();
    let media_type = axum::http::HeaderValue::from_str(&resource.content_type)
        .unwrap_or_else(|_| axum::http::HeaderValue::from_static("application/octet-stream"));
    let stream = DownloadStream {
        bytes: Bytes::from_owner(resource.body),
        _permit: permit,
        deadline: Box::pin(tokio::time::sleep(state.body_timeout)),
        failed: false,
    };
    let mut response = Body::from_stream(stream).into_response();
    let headers = response.headers_mut();
    headers.insert(axum::http::header::CONTENT_TYPE, media_type);
    headers.insert(axum::http::header::CONTENT_LENGTH, length.into());
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("private, no-store"),
    );
    headers.insert(
        axum::http::header::CONTENT_DISPOSITION,
        axum::http::HeaderValue::from_static("attachment; filename=\"gitea-result\""),
    );
    headers.insert(
        "x-content-type-options",
        axum::http::HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        "gitea-sensitive-result",
        axum::http::HeaderValue::from_static(if resource.sensitive { "true" } else { "false" }),
    );
    response
}

struct DownloadStream {
    bytes: Bytes,
    _permit: tokio::sync::OwnedSemaphorePermit,
    deadline: std::pin::Pin<Box<tokio::time::Sleep>>,
    failed: bool,
}

impl futures_util::Stream for DownloadStream {
    type Item = Result<Bytes, std::io::Error>;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::{future::Future, task::Poll};
        if self.failed || self.bytes.is_empty() {
            return Poll::Ready(None);
        }
        if self.deadline.as_mut().poll(context).is_ready() {
            self.failed = true;
            self.bytes = Bytes::new();
            return Poll::Ready(Some(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "download deadline exceeded",
            ))));
        }
        let length = self.bytes.len().min(64 * 1024);
        Poll::Ready(Some(Ok(self.bytes.split_to(length))))
    }
}

fn apply_ingress_controls(router: Router, settings: &Settings) -> Router {
    router
        .layer(middleware::from_fn_with_state(
            settings.identity.clone(),
            crate::identity::authenticate,
        ))
        .layer(middleware::from_fn_with_state(
            IngressState {
                slots: Arc::new(Semaphore::new(settings.max_concurrent_requests)),
                max_bytes: settings.max_request_bytes,
                body_timeout: settings.body_timeout,
                allowed_origins: settings.allowed_origins.clone(),
            },
            enforce_body_limit,
        ))
        .layer(BearerLayer::new(
            settings.gateway_bearer_current.clone(),
            settings.gateway_bearer_previous.clone(),
        ))
}

async fn enforce_body_limit(
    State(state): State<IngressState>,
    request: Request,
    next: Next,
) -> Response {
    let mut origins = request.headers().get_all(axum::http::header::ORIGIN).iter();
    if let Some(origin) = origins.next() {
        let allowed = origin
            .to_str()
            .ok()
            .and_then(crate::config::normalize_origin)
            .is_some_and(|origin| state.allowed_origins.contains(&origin));
        if !allowed || origins.next().is_some() {
            return file_refusal(StatusCode::FORBIDDEN, "gitea_origin_forbidden");
        }
    }
    // Refuse excess admission instead of accumulating an unbounded wait queue.
    let Ok(_permit) = state.slots.try_acquire() else {
        return file_refusal(StatusCode::SERVICE_UNAVAILABLE, "gitea_ingress_busy");
    };
    let method = match *request.method() {
        axum::http::Method::POST => "POST",
        axum::http::Method::GET => "GET",
        axum::http::Method::DELETE => "DELETE",
        _ => "other",
    };
    let (parts, body) = request.into_parts();
    let response =
        match tokio::time::timeout(state.body_timeout, to_bytes(body, state.max_bytes)).await {
            Ok(Ok(bytes)) => {
                next.run(Request::from_parts(parts, Body::from(bytes)))
                    .await
            }
            Ok(Err(_)) => StatusCode::PAYLOAD_TOO_LARGE.into_response(),
            Err(_) => file_refusal(StatusCode::REQUEST_TIMEOUT, "gitea_body_timeout"),
        };
    tracing::debug!(target: "gitea_server::diagnostics", %method,
        status = response.status().as_u16(), "MCP HTTP request completed");
    response
}

/// Probe the local health endpoint.
///
/// # Errors
///
/// Returns an error when the endpoint cannot be reached or is not healthy.
pub async fn healthcheck(host: &str, port: u16) -> anyhow::Result<()> {
    let host = if host == "0.0.0.0" { "127.0.0.1" } else { host };
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()?
        .get(format!("http://{host}:{port}/healthz"))
        .send()
        .await?;
    if response.status().is_success() {
        Ok(())
    } else {
        anyhow::bail!("health endpoint returned {}", response.status())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use axum::{http::Request, routing::post};
    use tokio::sync::{Notify, Semaphore};
    use tower::ServiceExt;

    use super::*;

    fn test_settings(previous: Option<&str>) -> Settings {
        Settings {
            upstream_url: "https://gitea.example.test".to_string(),
            service_token: "test-service-token".to_string(),
            token_credentials: None,
            gateway_bearer_current: "0123456789abcdef0123456789abcdef".to_string(),
            gateway_bearer_previous: previous.map(str::to_string),
            host: "127.0.0.1".to_string(),
            port: 8000,
            allowed_hosts: vec!["localhost".to_string()],
            allowed_origins: Vec::new(),
            timeout: Duration::from_secs(1),
            body_timeout: Duration::from_secs(1),
            max_request_bytes: 1024,
            max_concurrent_requests: 1,
            file_public_origin: None,
            identity: None,
            log_level: "info".to_string(),
        }
    }

    fn test_app(settings: &Settings) -> Router {
        let client = Arc::new(
            GiteaClient::new(
                &settings.upstream_url,
                &settings.service_token,
                settings.timeout,
            )
            .expect("client"),
        );
        let token_client = Arc::new(
            TokenLifecycleClient::new(
                &settings.upstream_url,
                "token-user",
                "token-password",
                settings.timeout,
            )
            .expect("token client"),
        );
        router(
            settings,
            client,
            token_client,
            None,
            &CancellationToken::new(),
        )
    }

    fn test_app_with_files(settings: &Settings, files: Arc<FilePlane>) -> Router {
        let client = Arc::new(
            GiteaClient::new(
                &settings.upstream_url,
                &settings.service_token,
                settings.timeout,
            )
            .expect("client"),
        );
        let token_client = Arc::new(
            TokenLifecycleClient::new(
                &settings.upstream_url,
                "token-user",
                "token-password",
                settings.timeout,
            )
            .expect("token client"),
        );
        router(
            settings,
            client,
            token_client,
            Some(files),
            &CancellationToken::new(),
        )
    }

    fn authenticated_request() -> Request<Body> {
        Request::post("/mcp")
            .header("host", "localhost")
            .header("authorization", "Bearer 0123456789abcdef0123456789abcdef")
            .body(Body::empty())
            .expect("request")
    }

    async fn mcp_status(bearer: Option<&str>, previous: Option<&str>) -> StatusCode {
        let settings = test_settings(previous);
        let app = test_app(&settings);
        let mut request = Request::post("/mcp").header("host", "localhost");
        if let Some(bearer) = bearer {
            request = request.header("authorization", format!("Bearer {bearer}"));
        }
        app.oneshot(request.body(Body::empty()).expect("request"))
            .await
            .expect("response")
            .status()
    }

    #[tokio::test]
    async fn downloads_require_the_transfer_credential_and_preserve_all_bytes() {
        let settings = test_settings(None);
        let files = FilePlane::new("http://localhost", Duration::from_secs(30)).unwrap();
        let store =
            gitea_mcp::resources::ResourceStore::new(gitea_mcp::resources::ResourceLimits {
                max_object_bytes: 16 * 1024 * 1024,
                max_total_bytes: 32 * 1024 * 1024,
                time_to_live: Duration::from_mins(1),
            });
        let resource = store
            .insert("logs", "text/plain", true, vec![b'x'; 10 * 1024 * 1024])
            .unwrap();
        let grant = files
            .authorize_download(
                &store,
                gitea_mcp::files::AuthorizeDownloadParams { uri: resource.uri },
            )
            .unwrap();
        let path = grant.download.url.strip_prefix("http://localhost").unwrap();
        let app = test_app_with_files(&settings, files);
        let refused = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        let downloaded = app
            .oneshot(
                Request::get(path)
                    .header(
                        TRANSFER_CREDENTIAL_HEADER,
                        &grant.download.headers[TRANSFER_CREDENTIAL_HEADER],
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(downloaded.status(), StatusCode::OK);
        assert_eq!(downloaded.headers()["gitea-sensitive-result"], "true");
        assert_eq!(downloaded.headers()["cache-control"], "private, no-store");
        let bytes = to_bytes(downloaded.into_body(), 16 * 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), resource.body.as_ref());
    }

    #[tokio::test]
    async fn download_capacity_is_held_until_body_completion_or_cancellation() {
        let slots = Arc::new(Semaphore::new(1));
        let stream = DownloadStream {
            bytes: Bytes::from_static(b"fixture"),
            _permit: slots.clone().try_acquire_owned().unwrap(),
            deadline: Box::pin(tokio::time::sleep(Duration::from_secs(1))),
            failed: false,
        };
        let body = Body::from_stream(stream);
        assert!(slots.try_acquire().is_err());
        drop(body);
        assert_eq!(slots.available_permits(), 1);
        let stream = DownloadStream {
            bytes: Bytes::from_static(b"fixture"),
            _permit: slots.clone().try_acquire_owned().unwrap(),
            deadline: Box::pin(tokio::time::sleep(Duration::from_millis(1))),
            failed: false,
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(to_bytes(Body::from_stream(stream), 100).await.is_err());
        assert_eq!(slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn one_time_transfer_credential_authorizes_upload_without_gateway_bearer() {
        let settings = test_settings(None);
        let files = FilePlane::new("https://gitea.example.test", Duration::from_mins(1))
            .expect("file plane");
        let authorized = files
            .authorize_upload(gitea_mcp::files::AuthorizeUploadParams::default())
            .expect("authorization");
        let id = authorized.upload.url.rsplit('/').next().expect("upload id");
        let credential = &authorized.upload.headers[TRANSFER_CREDENTIAL_HEADER];
        let request = Request::put(format!("/files/upload/{id}"))
            .header(TRANSFER_CREDENTIAL_HEADER, credential)
            .body(Body::from("fixture-value-never-production"))
            .expect("request");

        let response = test_app_with_files(&settings, Arc::clone(&files))
            .oneshot(request)
            .await
            .expect("upload response");

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let secret = files
            .take_secret(&authorized.file.uri)
            .expect("staged secret");
        assert_eq!(secret.as_str(), "fixture-value-never-production");
    }

    #[tokio::test]
    async fn upload_authentication_precedes_body_read_and_concurrency() {
        let settings = test_settings(None);
        let files = FilePlane::new("https://gitea.example.test", Duration::from_mins(1))
            .expect("file plane");
        let authorized = files
            .authorize_upload(gitea_mcp::files::AuthorizeUploadParams::default())
            .expect("authorization");
        let id = authorized.upload.url.rsplit('/').next().expect("upload id");
        let pending = futures_util::stream::pending::<Result<axum::body::Bytes, Infallible>>();
        let request = Request::put(format!("/files/upload/{id}"))
            .header(TRANSFER_CREDENTIAL_HEADER, "wrong")
            .body(Body::from_stream(pending))
            .expect("request");

        let response = tokio::time::timeout(
            Duration::from_millis(50),
            test_app_with_files(&settings, Arc::clone(&files)).oneshot(request),
        )
        .await
        .expect("bogus credential was rejected before reading its body")
        .expect("response");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let credential = &authorized.upload.headers[TRANSFER_CREDENTIAL_HEADER];
        let retry = Request::put(format!("/files/upload/{id}"))
            .header(TRANSFER_CREDENTIAL_HEADER, credential)
            .body(Body::from("fixture-value-never-production"))
            .expect("request");
        let response = test_app_with_files(&settings, Arc::clone(&files))
            .oneshot(retry)
            .await
            .expect("upload response");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn authorized_upload_body_has_a_read_deadline() {
        let mut settings = test_settings(None);
        settings.timeout = Duration::from_millis(20);
        let files = FilePlane::new("https://gitea.example.test", Duration::from_mins(1))
            .expect("file plane");
        let authorized = files
            .authorize_upload(gitea_mcp::files::AuthorizeUploadParams::default())
            .expect("authorization");
        let id = authorized.upload.url.rsplit('/').next().expect("upload id");
        let credential = &authorized.upload.headers[TRANSFER_CREDENTIAL_HEADER];
        let pending = futures_util::stream::pending::<Result<axum::body::Bytes, Infallible>>();
        let request = Request::put(format!("/files/upload/{id}"))
            .header(TRANSFER_CREDENTIAL_HEADER, credential)
            .body(Body::from_stream(pending))
            .expect("request");

        let response = test_app_with_files(&settings, files)
            .oneshot(request)
            .await
            .expect("upload response");
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    }

    #[tokio::test]
    async fn mcp_rejects_requests_without_gateway_bearer() {
        assert_eq!(mcp_status(None, None).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn mcp_accepts_current_gateway_bearer() {
        assert_ne!(
            mcp_status(Some("0123456789abcdef0123456789abcdef"), None).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn mcp_accepts_previous_gateway_bearer() {
        assert_ne!(
            mcp_status(
                Some("fedcba9876543210fedcba9876543210"),
                Some("fedcba9876543210fedcba9876543210"),
            )
            .await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn mcp_rejects_oversized_authenticated_request_before_parsing() {
        let settings = test_settings(None);
        let app = test_app(&settings);
        let response = app
            .oneshot(
                Request::post("/mcp")
                    .header("host", "localhost")
                    .header("authorization", "Bearer 0123456789abcdef0123456789abcdef")
                    .header("accept", "application/json, text/event-stream")
                    .header("content-type", "application/json")
                    .body(Body::from(vec![b'x'; settings.max_request_bytes + 1]))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn mcp_authenticates_before_buffering_request_body() {
        let settings = test_settings(None);
        let app = test_app(&settings);
        let response = app
            .oneshot(
                Request::post("/mcp")
                    .header("host", "localhost")
                    .header("accept", "application/json, text/event-stream")
                    .header("content-type", "application/json")
                    .body(Body::from(vec![b'x'; settings.max_request_bytes + 1]))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    struct ConcurrencyProbe {
        entered: AtomicUsize,
        entered_notify: Notify,
        release: Semaphore,
    }

    async fn blocking_handler(State(probe): State<Arc<ConcurrencyProbe>>) -> StatusCode {
        probe.entered.fetch_add(1, Ordering::SeqCst);
        probe.entered_notify.notify_one();
        probe.release.acquire().await.expect("probe open").forget();
        StatusCode::OK
    }

    #[tokio::test]
    async fn ingress_limits_aggregate_request_concurrency() {
        let probe = Arc::new(ConcurrencyProbe {
            entered: AtomicUsize::new(0),
            entered_notify: Notify::new(),
            release: Semaphore::new(0),
        });
        let settings = test_settings(None);
        let app = apply_ingress_controls(
            Router::new()
                .route("/mcp", post(blocking_handler))
                .with_state(probe.clone()),
            &settings,
        );

        let first = tokio::spawn(app.clone().oneshot(authenticated_request()));
        while probe.entered.load(Ordering::SeqCst) < 1 {
            probe.entered_notify.notified().await;
        }
        let second = app
            .clone()
            .oneshot(authenticated_request())
            .await
            .expect("response");
        assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(probe.entered.load(Ordering::SeqCst), 1);
        probe.release.add_permits(1);
        assert_eq!(
            first
                .await
                .expect("first task")
                .expect("first response")
                .status(),
            StatusCode::OK
        );
        probe.release.add_permits(1);
        assert_eq!(
            app.oneshot(authenticated_request())
                .await
                .expect("response")
                .status(),
            StatusCode::OK
        );
        assert_eq!(probe.entered.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn origin_policy_applies_to_every_mcp_method_before_body_read() {
        let mut settings = test_settings(None);
        settings.allowed_origins = vec!["https://client.example.test".to_string()];
        let app = apply_ingress_controls(
            Router::new().route("/mcp", axum::routing::any(|| async { StatusCode::OK })),
            &settings,
        );
        for method in ["POST", "GET", "DELETE"] {
            for (origin, expected) in [
                (None, StatusCode::OK),
                (Some("https://client.example.test"), StatusCode::OK),
                (Some("https://other.example.test"), StatusCode::FORBIDDEN),
                (Some("null"), StatusCode::FORBIDDEN),
                (Some("malformed"), StatusCode::FORBIDDEN),
                (
                    Some("https://client.example.test/path"),
                    StatusCode::FORBIDDEN,
                ),
            ] {
                let mut request = Request::builder()
                    .method(method)
                    .uri("/mcp")
                    .header("authorization", "Bearer 0123456789abcdef0123456789abcdef");
                if let Some(origin) = origin {
                    request = request.header("origin", origin);
                }
                let response = app
                    .clone()
                    .oneshot(request.body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(response.status(), expected);
            }
        }
        for origins in [
            vec!["https://client.example.test", "https://other.example.test"],
            vec!["https://client.example.test", "https://client.example.test"],
        ] {
            let mut request = authenticated_request();
            for origin in origins {
                request
                    .headers_mut()
                    .append("origin", origin.parse().unwrap());
            }
            assert_eq!(
                app.clone().oneshot(request).await.unwrap().status(),
                StatusCode::FORBIDDEN
            );
        }
        let app = apply_ingress_controls(
            Router::new().route("/mcp", post(|| async { StatusCode::OK })),
            &test_settings(None),
        );
        let mut request = authenticated_request();
        request
            .headers_mut()
            .insert("origin", "https://client.example.test".parse().unwrap());
        assert_eq!(
            app.oneshot(request).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn stalled_body_releases_admission_on_deadline_and_cancellation() {
        let mut settings = test_settings(None);
        settings.body_timeout = Duration::from_millis(30);
        let app = apply_ingress_controls(
            Router::new().route("/mcp", post(|| async { StatusCode::OK })),
            &settings,
        );
        let entered = Arc::new(Notify::new());
        let pending_request = |entered: Arc<Notify>| {
            let mut request = authenticated_request();
            *request.body_mut() = Body::from_stream(futures_util::stream::poll_fn(move |_| {
                entered.notify_one();
                std::task::Poll::Pending::<Option<Result<Bytes, Infallible>>>
            }));
            request
        };
        let first = tokio::spawn(app.clone().oneshot(pending_request(Arc::clone(&entered))));
        entered.notified().await;
        assert_eq!(
            app.clone()
                .oneshot(authenticated_request())
                .await
                .unwrap()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), first)
                .await
                .expect("body read deadline")
                .unwrap()
                .unwrap()
                .status(),
            StatusCode::REQUEST_TIMEOUT
        );
        assert_eq!(
            app.clone()
                .oneshot(authenticated_request())
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let abandoned = tokio::spawn(app.clone().oneshot(pending_request(Arc::clone(&entered))));
        entered.notified().await;
        abandoned.abort();
        assert!(abandoned.await.unwrap_err().is_cancelled());
        assert_eq!(
            app.oneshot(authenticated_request()).await.unwrap().status(),
            StatusCode::OK
        );
    }

    fn rpc_request(value: &serde_json::Value, session: Option<&str>) -> Request<Body> {
        let mut request = authenticated_request();
        for (name, value) in [
            ("content-type", "application/json"),
            ("accept", "application/json, text/event-stream"),
            ("mcp-protocol-version", "2025-11-25"),
        ] {
            request.headers_mut().insert(name, value.parse().unwrap());
        }
        if let Some(session) = session {
            request
                .headers_mut()
                .insert("mcp-session-id", session.parse().unwrap());
        }
        *request.body_mut() = Body::from(serde_json::to_vec(value).unwrap());
        request
    }

    async fn rpc_json(response: Response) -> serde_json::Value {
        let body = tokio::time::timeout(
            Duration::from_secs(5),
            to_bytes(response.into_body(), 256 * 1024),
        )
        .await
        .expect("bounded MCP response")
        .unwrap();
        if let Ok(value) = serde_json::from_slice(&body) {
            return value;
        }
        std::str::from_utf8(&body)
            .unwrap()
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .find_map(|line| serde_json::from_str(line).ok())
            .expect("SSE response")
    }

    async fn initialize_session(app: &Router) -> String {
        let response = app.clone().oneshot(rpc_request(&serde_json::json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{"protocolVersion":"2025-11-25", "capabilities":{}, "clientInfo":{"name":"runtime-test", "version":"1"}}}), None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let session = response.headers()["mcp-session-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let _ = rpc_json(response).await;
        let response = app
            .clone()
            .oneshot(rpc_request(
                &serde_json::json!({"jsonrpc":"2.0", "method":"notifications/initialized"}),
                Some(&session),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        session
    }

    #[tokio::test]
    async fn sse_execution_capacity_is_shared_across_sessions_until_upstream_completion() {
        let probe = Arc::new(ConcurrencyProbe {
            entered: AtomicUsize::new(0),
            entered_notify: Notify::new(),
            release: Semaphore::new(0),
        });
        let upstream = Router::new()
            .route(
                "/api/v1/version",
                get(|State(probe): State<Arc<ConcurrencyProbe>>| async move {
                    blocking_handler(State(probe)).await;
                    Json(serde_json::json!({"version":"1.26.4"}))
                }),
            )
            .with_state(Arc::clone(&probe));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut settings = test_settings(None);
        settings.upstream_url = format!("http://{}", listener.local_addr().unwrap());
        let upstream_task = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        let app = test_app(&settings);
        let session = initialize_session(&app).await;
        let other = initialize_session(&app).await;
        let version = serde_json::json!({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{"name":"server.version", "arguments":{}}});
        let first = app
            .clone()
            .oneshot(rpc_request(&version, Some(&session)))
            .await
            .unwrap();
        // The HTTP response is already available, while the tool is still active.
        tokio::time::timeout(Duration::from_secs(2), probe.entered_notify.notified())
            .await
            .expect("upstream work started");
        for (index, session) in [&session, &other].into_iter().enumerate() {
            let mut pending = version.clone();
            pending["id"] = serde_json::json!(10 + index);
            let response = app
                .clone()
                .oneshot(rpc_request(&pending, Some(session)))
                .await
                .unwrap();
            let result = rpc_json(response).await;
            assert_eq!(result["error"]["data"]["code"], "gitea_execution_busy");
            assert_eq!(result["error"]["data"]["outcome"], "not_sent");
        }
        let response = app.clone().oneshot(rpc_request(&serde_json::json!({"jsonrpc":"2.0", "id":3, "method":"resources/read", "params":{"uri":gitea_mcp::CATALOG_INDEX_URI}}), Some(&other))).await.unwrap();
        assert_eq!(
            rpc_json(response).await["error"]["data"]["code"],
            "gitea_execution_busy"
        );
        assert_eq!(probe.entered.load(Ordering::SeqCst), 1);
        // Cancelling the protocol request must not release capacity while the
        // submitted upstream request can still be executing.
        app.clone().oneshot(rpc_request(&serde_json::json!({"jsonrpc":"2.0", "method":"notifications/cancelled", "params":{"requestId":2}}), Some(&session))).await.unwrap();
        let response = app
            .clone()
            .oneshot(rpc_request(&version, Some(&other)))
            .await
            .unwrap();
        assert_eq!(
            rpc_json(response).await["error"]["data"]["code"],
            "gitea_execution_busy"
        );
        probe.release.add_permits(1);
        // Cancellation may close SSE without a result; it does not settle the
        // submitted upstream request. Wait for execution capacity to recover.
        let _ = to_bytes(first.into_body(), 256 * 1024).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let response = app.clone().oneshot(rpc_request(&serde_json::json!({"jsonrpc":"2.0", "id":20, "method":"resources/read", "params":{"uri":gitea_mcp::CATALOG_INDEX_URI}}), Some(&other))).await.unwrap();
                if rpc_json(response).await.get("result").is_some() { break; }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.expect("capacity released after upstream completion");
        probe.release.add_permits(1);
        let response = app
            .oneshot(rpc_request(&version, Some(&other)))
            .await
            .unwrap();
        assert!(rpc_json(response).await.get("result").is_some());
        assert_eq!(probe.entered.load(Ordering::SeqCst), 2);
        upstream_task.abort();
    }

    #[tokio::test]
    async fn authentication_and_origin_refusals_never_poll_the_body() {
        let app = test_app(&test_settings(None));
        for authenticated in [false, true] {
            let mut request = authenticated_request();
            if !authenticated {
                request.headers_mut().remove("authorization");
            }
            request.headers_mut().insert(
                "origin",
                axum::http::HeaderValue::from_bytes(b"\xff").unwrap(),
            );
            *request.body_mut() =
                Body::from_stream(futures_util::stream::pending::<Result<Bytes, Infallible>>());
            let response =
                tokio::time::timeout(Duration::from_millis(50), app.clone().oneshot(request))
                    .await
                    .expect("refused before buffering")
                    .unwrap();
            assert_eq!(
                response.status(),
                if authenticated {
                    StatusCode::FORBIDDEN
                } else {
                    StatusCode::UNAUTHORIZED
                }
            );
        }
    }
}
