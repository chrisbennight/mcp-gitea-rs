//! Verify the gateway's caller assertion separately from the ingress bearer.

use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use gitea_mcp::ResourceOwner;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use serde::Deserialize;
use tokio::sync::Mutex;

const MAX_JWKS_BYTES: usize = 64 * 1024;
const CACHE_TTL: Duration = Duration::from_mins(1);

#[derive(Clone)]
pub struct IdentityVerifier {
    issuer: String,
    actor: String,
    jwks_url: url::Url,
    client: reqwest::Client,
    cache: Arc<Mutex<Option<CachedKeys>>>,
}

struct CachedKeys {
    keys: Option<JwkSet>,
    refreshed: Instant,
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
    original_issuer: Option<String>,
    iat: u64,
    exp: u64,
    act: Actor,
}

#[derive(Deserialize)]
struct Actor {
    sub: String,
}

impl IdentityVerifier {
    /// Build a verifier for the configured gateway's Ed25519 assertions.
    ///
    /// # Errors
    /// Returns a fixed diagnostic for invalid configuration or client setup.
    pub fn new(jwks_url: &str, issuer: String, actor: String) -> Result<Self, &'static str> {
        let jwks_url = url::Url::parse(jwks_url).map_err(|_| "invalid identity JWKS URL")?;
        let private_http = match jwks_url.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback() || ip.is_private(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback() || ip.is_unique_local(),
            Some(url::Host::Domain(host)) => !host.contains('.'),
            None => false,
        };
        if !(jwks_url.scheme() == "https" || jwks_url.scheme() == "http" && private_http)
            || !jwks_url.username().is_empty()
            || jwks_url.password().is_some()
            || jwks_url.query().is_some()
            || jwks_url.fragment().is_some()
            || ResourceOwner::verified(&issuer, &actor).is_none()
        {
            return Err("invalid identity issuer, actor, or JWKS URL");
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| "could not initialize identity verifier")?;
        Ok(Self {
            issuer,
            actor,
            jwks_url,
            client,
            cache: Arc::new(Mutex::new(None)),
        })
    }

    async fn keys(&self) -> Option<JwkSet> {
        // Serialize refreshes and cache failures too. Unknown key IDs cannot
        // force unbounded requests to the configured key service.
        let mut cache = self.cache.lock().await;
        if let Some(cached) = cache
            .as_ref()
            .filter(|cached| cached.refreshed.elapsed() < CACHE_TTL)
        {
            return cached.keys.clone();
        }
        let keys = self.fetch_keys().await;
        *cache = Some(CachedKeys {
            keys: keys.clone(),
            refreshed: Instant::now(),
        });
        keys
    }

    async fn fetch_keys(&self) -> Option<JwkSet> {
        let mut response = self.client.get(self.jwks_url.clone()).send().await.ok()?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|size| size > MAX_JWKS_BYTES as u64)
        {
            return None;
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if chunk.len() > MAX_JWKS_BYTES.saturating_sub(body.len()) {
                return None;
            }
            body.extend_from_slice(&chunk);
        }
        let keys: JwkSet = serde_json::from_slice(&body).ok()?;
        (keys.keys.len() <= 32).then_some(keys)
    }

    async fn verify(&self, token: &str) -> Option<ResourceOwner> {
        if token.len() > 16 * 1024 {
            return None;
        }
        let header = decode_header(token).ok()?;
        if header.alg != Algorithm::EdDSA {
            return None;
        }
        let kid = header.kid?;
        if kid.len() > 256 {
            return None;
        }
        let keys = self.keys().await?;
        let key = DecodingKey::from_jwk(keys.find(&kid)?).ok()?;
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&["gitea"]);
        validation.set_required_spec_claims(&["iss", "sub", "aud", "iat", "exp"]);
        validation.leeway = 30;
        let claims = decode::<Claims>(token, &key, &validation).ok()?.claims;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        if claims.act.sub != self.actor
            || claims.iat > now.saturating_add(30)
            || claims.iat < now.saturating_sub(270)
            || !claims
                .exp
                .checked_sub(claims.iat)
                .is_some_and(|age| (1..=300).contains(&age))
        {
            return None;
        }
        ResourceOwner::verified(
            claims.original_issuer.as_deref().unwrap_or(&self.issuer),
            &claims.sub,
        )
    }
}

pub(crate) async fn authenticate(
    State(verifier): State<Option<IdentityVerifier>>,
    mut request: Request,
    next: Next,
) -> Response {
    if let Some(verifier) = verifier {
        let mut headers = request.headers().get_all("x-mcp-identity").iter();
        let token = headers.next().and_then(|value| value.to_str().ok());
        if headers.next().is_some() {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let Some(token) = token else {
            return StatusCode::UNAUTHORIZED.into_response();
        };
        let Some(owner) = verifier.verify(token).await else {
            return StatusCode::UNAUTHORIZED.into_response();
        };
        request.extensions_mut().insert(owner);
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::get};
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde_json::{Value, json};
    use tokio_util::sync::CancellationToken;

    const BEARER: &str = "synthetic-current-bearer-32-bytes-long";
    const PREVIOUS: &str = "synthetic-previous-bearer-32-bytes-long";

    fn token(subject: &str, changes: Value) -> String {
        // Public RFC 8032 test vector, used only by loopback fixtures.
        let der = "302e020100300506032b6570042204209d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
        let der: Vec<u8> = (0..der.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&der[i..i + 2], 16).unwrap())
            .collect();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut claims = json!({"iss":"https://gateway.test", "sub":subject, "aud":"gitea", "iat":now, "exp":now+120, "act":{"sub":"gateway.test"}, "original_issuer":"https://idp.test"});
        let Value::Object(changes) = changes else {
            panic!("fixture overrides must be an object");
        };
        claims.as_object_mut().unwrap().extend(changes);
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some("fixture".into());
        encode(&header, &claims, &EncodingKey::from_ed_der(&der)).unwrap()
    }

    async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (origin, task)
    }

    async fn verifier() -> (IdentityVerifier, tokio::task::JoinHandle<()>) {
        let (origin, task) = serve(Router::new().route("/jwks", get(|| async {
            Json(json!({"keys":[{"kty":"OKP", "crv":"Ed25519", "alg":"EdDSA", "use":"sig", "kid":"fixture", "x":"11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"}]}))
        }))).await;
        (
            IdentityVerifier::new(
                &format!("{origin}/jwks"),
                "https://gateway.test".into(),
                "gateway.test".into(),
            )
            .unwrap(),
            task,
        )
    }

    #[tokio::test]
    async fn verifies_signature_issuer_audience_actor_and_time() {
        let (verifier, task) = verifier().await;
        assert_eq!(
            verifier.verify(&token("alice", json!({}))).await,
            ResourceOwner::verified("https://idp.test", "alice")
        );
        assert_eq!(
            verifier
                .verify(&token("alice", json!({"original_issuer":null})))
                .await,
            ResourceOwner::verified("https://gateway.test", "alice")
        );
        for changes in [
            json!({"iss":"https://other.test"}),
            json!({"aud":"other"}),
            json!({"act":{"sub":"other"}}),
            json!({"exp":1}),
            json!({"iat":1}),
            json!({"sub":""}),
            json!({"original_issuer":""}),
            json!({"exp":u64::MAX}),
            json!({"iat":u64::MAX}),
        ] {
            assert!(verifier.verify(&token("alice", changes)).await.is_none());
        }
        let signed = token("alice", json!({}));
        let mut tampered = signed.into_bytes();
        let index = tampered.iter().rposition(|byte| *byte == b'.').unwrap() + 1;
        tampered[index] = if tampered[index] == b'A' { b'B' } else { b'A' };
        assert!(
            verifier
                .verify(std::str::from_utf8(&tampered).unwrap())
                .await
                .is_none()
        );
        assert!(verifier.verify("invalid").await.is_none());
        task.abort();
    }

    struct Rpc {
        client: reqwest::Client,
        endpoint: String,
    }

    impl Rpc {
        async fn call(
            &self,
            identity: &str,
            bearer: &str,
            session: Option<&str>,
            method: &str,
            params: Value,
        ) -> (reqwest::header::HeaderMap, Value) {
            let mut request = self
                .client
                .post(&self.endpoint)
                .bearer_auth(bearer)
                .header("x-mcp-identity", identity)
                .header("accept", "application/json, text/event-stream")
                .header("mcp-protocol-version", "2025-11-25")
                .json(&json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params}));
            if let Some(session) = session {
                request = request.header("mcp-session-id", session);
            }
            let response = request.send().await.unwrap();
            assert!(response.status().is_success());
            let headers = response.headers().clone();
            let text = response.text().await.unwrap();
            let value = serde_json::from_str(&text).unwrap_or_else(|_| {
                text.lines()
                    .filter_map(|line| line.strip_prefix("data:"))
                    .find_map(|line| serde_json::from_str(line).ok())
                    .unwrap()
            });
            (headers, value)
        }

        async fn initialize(&self, identity: &str, bearer: &str) -> String {
            let (headers, _) = self.call(identity, bearer, None, "initialize", json!({"protocolVersion":"2025-11-25", "capabilities":{}, "clientInfo":{"name":"fixture", "version":"1"}})).await;
            let session = headers["mcp-session-id"].to_str().unwrap().to_string();
            let response = self
                .client
                .post(&self.endpoint)
                .bearer_auth(bearer)
                .header("x-mcp-identity", identity)
                .header("mcp-session-id", &session)
                .header("accept", "application/json, text/event-stream")
                .header("mcp-protocol-version", "2025-11-25")
                .json(&json!({"jsonrpc":"2.0", "method":"notifications/initialized"}))
                .send()
                .await
                .unwrap();
            assert!(response.status().is_success());
            session
        }
    }

    struct Fixture {
        rpc: Rpc,
        cancel: CancellationToken,
        tasks: [tokio::task::JoinHandle<()>; 3],
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.cancel.cancel();
            for task in &self.tasks {
                task.abort();
            }
        }
    }

    async fn fixture() -> Fixture {
        let (identity, keys_task) = verifier().await;
        let (upstream, upstream_task) = serve(Router::new().route(
            "/api/v1/repos/fixture/fixture/actions/jobs/1/logs",
            get(|| async { "fixture log line\n".repeat(8192) }),
        ))
        .await;
        let settings = crate::config::Settings {
            upstream_url: upstream.clone(),
            service_token: "fixture".into(),
            token_credentials: None,
            gateway_bearer_current: BEARER.into(),
            gateway_bearer_previous: Some(PREVIOUS.into()),
            host: "127.0.0.1".into(),
            port: 0,
            allowed_hosts: vec!["127.0.0.1".into()],
            allowed_origins: vec![],
            timeout: Duration::from_secs(5),
            body_timeout: Duration::from_secs(5),
            max_request_bytes: 1024 * 1024,
            max_concurrent_requests: 8,
            file_public_origin: Some("http://127.0.0.1".into()),
            identity: Some(identity),
            log_level: "info".into(),
        };
        let client = Arc::new(
            gitea_api::GiteaClient::new(&upstream, "fixture", Duration::from_secs(5)).unwrap(),
        );
        let files =
            gitea_mcp::files::FilePlane::new("http://127.0.0.1", Duration::from_secs(30)).unwrap();
        let cancel = CancellationToken::new();
        let (origin, server_task) = serve(crate::server::router(
            &settings,
            client,
            None,
            Some(files),
            &cancel,
        ))
        .await;
        let rpc = Rpc {
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            endpoint: format!("{origin}/mcp"),
        };
        Fixture {
            rpc,
            cancel,
            tasks: [server_task, upstream_task, keys_task],
        }
    }

    #[tokio::test]
    async fn retained_reads_selection_listing_and_downloads_follow_identity_across_sessions() {
        let fixture = fixture().await;
        let rpc = &fixture.rpc;
        for bad in [None, Some("unverified")] {
            let mut request = rpc.client.post(&rpc.endpoint).bearer_auth(BEARER);
            if let Some(bad) = bad {
                request = request.header("x-mcp-identity", bad);
            }
            assert_eq!(
                request.send().await.unwrap().status(),
                StatusCode::UNAUTHORIZED
            );
        }
        let alice = token("alice", json!({}));
        let first = rpc.initialize(&alice, BEARER).await;
        let (_, result) = rpc.call(&alice, BEARER, Some(&first), "tools/call", json!({"name":"api.read", "arguments":{"operation_id":"downloadActionsRunJobLogs", "arguments":{"owner":"fixture", "repo":"fixture", "job_id":1}}})).await;
        let uri = result["result"]["structuredContent"]["payload"]["resource_uri"]
            .as_str()
            .unwrap();
        assert!(
            rpc.client
                .delete(&rpc.endpoint)
                .bearer_auth(BEARER)
                .header("x-mcp-identity", &alice)
                .header("mcp-session-id", &first)
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
        let reissued = token(
            "alice",
            json!({"exp":SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()+180}),
        );
        let second = rpc.initialize(&reissued, PREVIOUS).await;
        for (identity, owns) in [
            (&reissued, true),
            (&token("bob", json!({})), false),
            (
                &token("alice", json!({"original_issuer":"https://other-idp.test"})),
                false,
            ),
        ] {
            // The current request identity wins even when a caller presents an
            // existing transport session identifier belonging to another user.
            for (method, params) in [
                ("resources/read", json!({"uri":uri})),
                (
                    "tools/call",
                    json!({"name":"result.select", "arguments":{"uri":uri, "mode":"search", "text":"fixture"}}),
                ),
                ("files/authorizeDownload", json!({"uri":uri})),
            ] {
                let (_, reply) = rpc
                    .call(identity, PREVIOUS, Some(&second), method, params)
                    .await;
                assert_eq!(reply.get("error").is_none(), owns, "{method}");
            }
            let (_, listed) = rpc
                .call(
                    identity,
                    PREVIOUS,
                    Some(&second),
                    "resources/list",
                    json!({}),
                )
                .await;
            assert_eq!(listed.to_string().contains(uri), owns);
        }
    }
}
