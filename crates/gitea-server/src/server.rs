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
use tower::limit::ConcurrencyLimitLayer;
use tower_http::trace::TraceLayer;
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
    token_client: Arc<TokenLifecycleClient>,
    files: Option<Arc<FilePlane>>,
    cancellation: &CancellationToken,
) -> Router {
    let mcp = GiteaMcp::new(client, token_client).with_files(files.clone());
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
                .with_state(upload_state),
        );
    }
    router.layer(TraceLayer::new_for_http())
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
                FileError::Unauthorized | FileError::Expired | FileError::Unavailable => {
                    StatusCode::FORBIDDEN
                }
                FileError::TooManyOutstanding => StatusCode::SERVICE_UNAVAILABLE,
                FileError::EntropyUnavailable => StatusCode::INTERNAL_SERVER_ERROR,
                FileError::TooLarge | FileError::SizeMismatch => StatusCode::PAYLOAD_TOO_LARGE,
                FileError::DigestMismatch
                | FileError::UnsupportedDigest
                | FileError::InvalidMetadata
                | FileError::InvalidSecret => StatusCode::BAD_REQUEST,
            };
            tracing::warn!(code = error.code(), "refused a file upload");
            file_refusal(status, error.code())
        }
    }
}

fn file_refusal(status: StatusCode, code: &str) -> Response {
    (status, Json(serde_json::json!({"error": code}))).into_response()
}

fn apply_ingress_controls(router: Router, settings: &Settings) -> Router {
    router
        .layer(middleware::from_fn_with_state(
            settings.max_request_bytes,
            enforce_body_limit,
        ))
        .layer(ConcurrencyLimitLayer::new(settings.max_concurrent_requests))
        .layer(BearerLayer::new(
            settings.gateway_bearer_current.clone(),
            settings.gateway_bearer_previous.clone(),
        ))
}

async fn enforce_body_limit(
    State(max_bytes): State<usize>,
    request: Request,
    next: Next,
) -> Response {
    let (parts, body) = request.into_parts();
    match to_bytes(body, max_bytes).await {
        Ok(bytes) => {
            next.run(Request::from_parts(parts, Body::from(bytes)))
                .await
        }
        Err(_) => StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    }
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
            token_username: "token-user".to_string(),
            token_password: "token-password".to_string(),
            gateway_bearer_current: "0123456789abcdef0123456789abcdef".to_string(),
            gateway_bearer_previous: previous.map(str::to_string),
            host: "127.0.0.1".to_string(),
            port: 8000,
            allowed_hosts: vec!["localhost".to_string()],
            timeout: Duration::from_secs(1),
            max_request_bytes: 1024,
            max_concurrent_requests: 1,
            file_public_origin: None,
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
                &settings.token_username,
                &settings.token_password,
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
                &settings.token_username,
                &settings.token_password,
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
        let second = tokio::spawn(app.oneshot(authenticated_request()));
        let second_entered = tokio::time::timeout(Duration::from_millis(50), async {
            while probe.entered.load(Ordering::SeqCst) < 2 {
                probe.entered_notify.notified().await;
            }
        })
        .await;
        assert!(
            second_entered.is_err(),
            "the second request entered before a concurrency permit was released"
        );

        probe.release.add_permits(2);
        assert_eq!(
            first
                .await
                .expect("first task")
                .expect("first response")
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            second
                .await
                .expect("second task")
                .expect("second response")
                .status(),
            StatusCode::OK
        );
    }
}
