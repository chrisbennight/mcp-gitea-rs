use std::{process::Stdio, time::Duration};

use axum::{
    Json, Router,
    routing::{get, post},
};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::Command,
};

const MARKER: &str = "synthetic-credential-log-fixture";
const BEARER: &str = "synthetic-ingress-bearer-at-least-32-bytes";

async fn rpc(
    client: &Client,
    endpoint: &str,
    session: Option<&str>,
    message: Value,
) -> (reqwest::header::HeaderMap, Value) {
    let mut request = client
        .post(endpoint)
        .bearer_auth(BEARER)
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-11-25")
        .json(&message);
    if let Some(session) = session {
        request = request.header("mcp-session-id", session);
    }
    let response = request.send().await.expect("loopback response");
    assert!(response.status().is_success());
    let headers = response.headers().clone();
    let text = response.text().await.expect("response body");
    let value = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or_else(|_| {
            text.lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .find_map(|line| serde_json::from_str(line).ok())
                .expect("SSE result")
        })
    };
    (headers, value)
}

async fn exercise_protocol(client: &Client, endpoint: &str) -> Vec<String> {
    let (headers, _) = rpc(client, endpoint, None, json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{
        "protocolVersion":"2025-11-25", "capabilities":{}, "clientInfo":{"name":MARKER,"version":"1"}
    }})).await;
    let session = headers["mcp-session-id"].to_str().unwrap();
    rpc(
        client,
        endpoint,
        Some(session),
        json!({"jsonrpc":"2.0", "method":"notifications/initialized"}),
    )
    .await;
    let (_, token) = rpc(client, endpoint, Some(session), json!({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{
        "name":"access_token.create", "arguments":{"name":"fixture", "scopes":["read:repository"]}
    }})).await;
    assert!(token.get("result").is_some());
    assert_ne!(token["result"]["isError"], true);
    assert!(token.to_string().contains(MARKER), "fixture minted a token");
    // Invalid tool arguments, unknown methods, and notification fields all
    // exercise SDK diagnostic paths with data that must never be logged.
    rpc(
        client,
        endpoint,
        Some(session),
        json!({"jsonrpc":"2.0", "id":3, "method":"tools/call", "params":{
            "name":"server.version", "arguments":{"secret":MARKER}
        }}),
    )
    .await;
    rpc(
        client,
        endpoint,
        Some(session),
        json!({"jsonrpc":"2.0", "id":4, "method":MARKER}),
    )
    .await;
    rpc(
        client,
        endpoint,
        Some(session),
        json!({"jsonrpc":"2.0", "method":"notifications/message", "params":{"data":MARKER}}),
    )
    .await;
    let (_, authorization) = rpc(
        client,
        endpoint,
        Some(session),
        json!({"jsonrpc":"2.0", "id":5, "method":"files/authorizeUpload", "params":{}}),
    )
    .await;
    let credentials = authorization["result"]["upload"]["headers"]
        .as_object()
        .expect("upload credentials")
        .values()
        .map(|value| value.as_str().unwrap().to_string())
        .collect();
    let (_, logs) = rpc(client, endpoint, Some(session), json!({"jsonrpc":"2.0", "id":6, "method":"tools/call", "params":{
        "name":"api.read", "arguments":{"operation_id":"downloadActionsRunJobLogs", "arguments":{"owner":"fixture", "repo":"fixture", "job_id":1}}
    }})).await;
    let uri = logs["result"]["structuredContent"]["payload"]["resource_uri"]
        .as_str()
        .expect("retained log");
    let (_, retained) = rpc(
        client,
        endpoint,
        Some(session),
        json!({"jsonrpc":"2.0", "id":7, "method":"resources/read", "params":{"uri":uri}}),
    )
    .await;
    assert!(retained.to_string().contains(MARKER));
    credentials
}

#[tokio::test]
async fn production_subscriber_keeps_sensitive_protocol_data_out_of_logs() {
    let upstream = Router::new()
        .route("/api/v1/users/fixture/tokens", post(|| async {
            (axum::http::StatusCode::CREATED, Json(json!({"id":1, "name":"fixture", "sha1":MARKER, "scopes":["read:repository"]})))
        }))
        .route("/api/v1/repos/fixture/fixture/actions/jobs/1/logs", get(|| async {
            MARKER.repeat(3000)
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    });
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    for level in [
        "info",
        "debug",
        "trace",
        "trace,rmcp=trace,reqwest=trace,tower_http=trace",
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_mcp-gitea-rs"))
            .env_clear()
            .env("GITEA_MCP_UPSTREAM_URL", &upstream_url)
            .env("GITEA_MCP_SERVICE_TOKEN", MARKER)
            .env("GITEA_MCP_TOKEN_USERNAME", "fixture")
            .env("GITEA_MCP_TOKEN_PASSWORD", MARKER)
            .env("GITEA_MCP_GATEWAY_BEARER_CURRENT", BEARER)
            .env("GITEA_MCP_HOST", "127.0.0.1")
            .env("GITEA_MCP_PORT", "0")
            .env("GITEA_MCP_ALLOWED_HOSTS", "127.0.0.1")
            .env("GITEA_MCP_FILE_PUBLIC_ORIGIN", "http://127.0.0.1")
            .env("GITEA_MCP_LOG_LEVEL", level)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut first_line = String::new();
        tokio::time::timeout(Duration::from_secs(10), stdout.read_line(&mut first_line))
            .await
            .unwrap()
            .unwrap();
        let started: Value = serde_json::from_str(&first_line).expect("safe startup event");
        let endpoint = format!(
            "http://{}/mcp",
            started["fields"]["address"]
                .as_str()
                .expect("bound address")
        );
        let mut stderr = child.stderr.take().unwrap().take(4 * 1024 * 1024);
        let capture = tokio::spawn(async move {
            let mut stdout = stdout.take(4 * 1024 * 1024);
            let mut out = first_line.into_bytes();
            let mut err = Vec::new();
            tokio::try_join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err)).unwrap();
            out.extend(err);
            out
        });
        let credentials = exercise_protocol(&client, &endpoint).await;
        child.kill().await.unwrap();
        let logs = capture.await.unwrap();
        for secret in [MARKER, BEARER]
            .into_iter()
            .chain(credentials.iter().map(String::as_str))
        {
            assert!(
                !logs
                    .windows(secret.len())
                    .any(|part| part == secret.as_bytes()),
                "sensitive fixture appeared in logs"
            );
        }
    }
    task.abort();
}
