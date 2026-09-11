use std::{
    env,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use gitea_api::{AccessTokenSelector, CreateAccessToken, GiteaClient, TokenLifecycleClient};
use gitea_mcp::GiteaMcp;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use serde_json::{Map, Value, json};
use tokio::{
    net::TcpListener,
    sync::{Barrier, oneshot},
    task::JoinHandle,
};

const ADMIN: &str = "scope-admin";
const ORGANIZATION: &str = "bootstrap-smoke";
const RACE_LOOKUP_PATH: &str = "/api/v1/repos/bootstrap-smoke/create-race";
const PROXY_BODY_LIMIT: usize = 1_048_576;

#[derive(Clone)]
struct RaceProxyState {
    upstream: String,
    client: reqwest::Client,
    lookup_count: Arc<AtomicUsize>,
    lookup_barrier: Arc<Barrier>,
}

struct RaceProxy {
    url: String,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<Result<(), std::io::Error>>,
}

#[tokio::main]
async fn main() -> ExitCode {
    match verify().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("repository bootstrap smoke failed: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn verify() -> Result<(), String> {
    let base_url = required("GITEA_BOOTSTRAP_TEST_URL")?;
    let password = required("GITEA_BOOTSTRAP_TEST_PASSWORD")?;
    let primary_key = required("GITEA_BOOTSTRAP_TEST_PRIMARY_KEY")?;
    let failure_key = required("GITEA_BOOTSTRAP_TEST_FAILURE_KEY")?;
    let token_client = Arc::new(
        TokenLifecycleClient::new(&base_url, ADMIN, &password, Duration::from_secs(10))
            .map_err(|error| error.to_string())?,
    );
    let service = token_client
        .create(&CreateAccessToken {
            name: "bootstrap-smoke-service".to_string(),
            scopes: vec![
                "read:user".to_string(),
                "write:issue".to_string(),
                "write:organization".to_string(),
                "write:repository".to_string(),
            ],
        })
        .await
        .map_err(|error| error.to_string())?;
    let client = Arc::new(
        GiteaClient::new(&base_url, &service.token, Duration::from_secs(10))
            .map_err(|error| error.to_string())?,
    );
    let mcp = GiteaMcp::new(client, Arc::clone(&token_client));

    let verification = verify_workflows(
        &base_url,
        &service.token,
        &mcp,
        &token_client,
        &primary_key,
        &failure_key,
    )
    .await;
    let cleanup = token_client
        .revoke(&AccessTokenSelector::Id(service.id))
        .await
        .map_err(|error| format!("service-token cleanup failed: {error}"));
    verification.and(cleanup)
}

async fn verify_workflows(
    base_url: &str,
    service_token: &str,
    mcp: &GiteaMcp,
    token_client: &Arc<TokenLifecycleClient>,
    primary_key: &str,
    failure_key: &str,
) -> Result<(), String> {
    create_organization_fixture(mcp).await?;
    let (agent_id, agent_mcp) =
        verify_complete_bootstrap(base_url, mcp, token_client, primary_key).await?;
    verify_repository_race(base_url, service_token, token_client).await?;
    let conflict_id = verify_compensated_failure(mcp, token_client, failure_key).await?;
    token_client
        .revoke(&AccessTokenSelector::Id(conflict_id))
        .await
        .map_err(|error| format!("conflict-token cleanup failed: {error}"))?;
    token_client
        .revoke(&AccessTokenSelector::Id(agent_id))
        .await
        .map_err(|error| format!("bootstrap-token cleanup failed: {error}"))?;
    verify_revoked(&agent_mcp).await
}

async fn create_organization_fixture(mcp: &GiteaMcp) -> Result<(), String> {
    invoke_success(
        mcp,
        "organization.create",
        object(json!({
            "organization": {
                "username": ORGANIZATION,
                "visibility": "private"
            }
        }))?,
    )
    .await?;
    invoke_success(
        mcp,
        "organization.create_team",
        object(json!({
            "org": ORGANIZATION,
            "body": {
                "name": "developers",
                "permission": "write",
                "units": ["repo.actions", "repo.code", "repo.issues", "repo.pulls"]
            }
        }))?,
    )
    .await?;
    Ok(())
}

async fn verify_complete_bootstrap(
    base_url: &str,
    mcp: &GiteaMcp,
    token_client: &Arc<TokenLifecycleClient>,
    primary_key: &str,
) -> Result<(i64, GiteaMcp), String> {
    let declaration = bootstrap_declaration(
        "complete",
        primary_key,
        "bootstrap-agent",
        &["write:repository"],
    );
    let first = bootstrap_success(mcp, declaration.clone()).await?;
    if first["repository_created"] != true || first["access_token"]["token"].as_str().is_none() {
        return Err("initial bootstrap did not create the repository and scoped token".to_string());
    }
    assert_operations(
        &first,
        &[
            "createOrgRepo",
            "repoEdit",
            "repoUpdateTopics",
            "repoAddCollaborator",
            "issueCreateLabel",
            "repoCreateHook",
            "repoCreateKey",
            "createRepoVariable",
            "updateRepoSecret",
            "repoCreateBranchProtection",
            "userCreateToken",
        ],
    )?;
    if !first["steps"].as_array().is_some_and(|steps| {
        steps.iter().any(|step| {
            matches!(
                step["operation_id"].as_str(),
                Some("repoAddTeam" | "repoCheckTeam")
            )
        })
    }) {
        return Err("bootstrap did not converge the declared team".to_string());
    }

    let agent_id = first["access_token"]["id"]
        .as_i64()
        .ok_or_else(|| "bootstrap token result has no identifier".to_string())?;
    let agent_token = first["access_token"]["token"]
        .as_str()
        .ok_or_else(|| "bootstrap token result has no value".to_string())?;
    let agent_mcp = GiteaMcp::new(
        Arc::new(
            GiteaClient::new(base_url, agent_token, Duration::from_secs(10))
                .map_err(|error| error.to_string())?,
        ),
        Arc::clone(token_client),
    );
    invoke_success(
        &agent_mcp,
        "repository.get",
        object(json!({"owner": ORGANIZATION, "repo": "complete"}))?,
    )
    .await?;

    let adopted = bootstrap_success(mcp, declaration).await?;
    if adopted["repository_created"] != false || !adopted["access_token"].is_null() {
        return Err("adoption retry recreated a repository or token".to_string());
    }
    Ok((agent_id, agent_mcp))
}

async fn verify_repository_race(
    base_url: &str,
    service_token: &str,
    token_client: &Arc<TokenLifecycleClient>,
) -> Result<(), String> {
    let proxy = start_race_proxy(base_url).await?;
    let client = Arc::new(
        GiteaClient::new(&proxy.url, service_token, Duration::from_secs(10))
            .map_err(|error| error.to_string())?,
    );
    let race_mcp = GiteaMcp::new(client, Arc::clone(token_client));
    let race = object(json!({
        "owner": ORGANIZATION,
        "owner_kind": "organization",
        "repository": {"name": "create-race", "private": true},
        "if_exists": "adopt"
    }))?;
    let (left, right) = tokio::join!(
        bootstrap_success(&race_mcp, race.clone()),
        bootstrap_success(&race_mcp, race)
    );
    proxy.stop().await?;
    let left = left?;
    let right = right?;
    let created = [&left, &right]
        .into_iter()
        .filter(|result| result["repository_created"] == true)
        .count();
    if created != 1 {
        return Err(format!(
            "concurrent create/adopt produced {created} repository creators"
        ));
    }
    let conflict_adoptions = [&left, &right]
        .into_iter()
        .filter(|result| {
            result["steps"].as_array().is_some_and(|steps| {
                steps
                    .iter()
                    .any(|step| step["detail"] == "repository race adopted")
            })
        })
        .count();
    if conflict_adoptions != 1 {
        return Err(format!(
            "concurrent create/adopt produced {conflict_adoptions} conflict rereads"
        ));
    }
    Ok(())
}

async fn start_race_proxy(upstream: &str) -> Result<RaceProxy, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| format!("race proxy bind failed: {error}"))?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("race proxy address failed: {error}"))?;
    let state = RaceProxyState {
        upstream: upstream.trim_end_matches('/').to_string(),
        client: reqwest::Client::new(),
        lookup_count: Arc::new(AtomicUsize::new(0)),
        lookup_barrier: Arc::new(Barrier::new(2)),
    };
    let router = Router::new()
        .route("/{*path}", any(forward_race_request))
        .with_state(state);
    let (shutdown, receiver) = oneshot::channel();
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = receiver.await;
            })
            .await
    });
    Ok(RaceProxy {
        url: format!("http://{address}"),
        shutdown,
        task,
    })
}

async fn forward_race_request(State(state): State<RaceProxyState>, request: Request) -> Response {
    let lookup = request.method() == reqwest::Method::GET
        && request.uri().path() == RACE_LOOKUP_PATH
        && state.lookup_count.fetch_add(1, Ordering::SeqCst) < 2;
    let response = forward_request(&state, request).await;
    if lookup {
        state.lookup_barrier.wait().await;
    }
    response
}

async fn forward_request(state: &RaceProxyState, request: Request) -> Response {
    let (mut parts, body) = request.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map_or("/", |value| value.as_str());
    let url = format!("{}{path}", state.upstream);
    parts.headers.remove(header::HOST);
    parts.headers.remove(header::CONTENT_LENGTH);
    let Ok(body) = to_bytes(body, PROXY_BODY_LIMIT).await else {
        return (StatusCode::BAD_REQUEST, "invalid proxy request").into_response();
    };
    let Ok(upstream) = state
        .client
        .request(parts.method, url)
        .headers(parts.headers)
        .body(body)
        .send()
        .await
    else {
        return (StatusCode::BAD_GATEWAY, "proxy forwarding failed").into_response();
    };
    let status = upstream.status();
    let content_type = upstream.headers().get(header::CONTENT_TYPE).cloned();
    let Ok(body) = upstream.bytes().await else {
        return (StatusCode::BAD_GATEWAY, "proxy response failed").into_response();
    };
    let mut response = Response::builder().status(status);
    if let Some(content_type) = content_type {
        response = response.header(header::CONTENT_TYPE, content_type);
    }
    response.body(Body::from(body)).unwrap_or_else(|_| {
        (StatusCode::INTERNAL_SERVER_ERROR, "proxy response invalid").into_response()
    })
}

impl RaceProxy {
    async fn stop(self) -> Result<(), String> {
        self.shutdown
            .send(())
            .map_err(|()| "race proxy stopped before cleanup".to_string())?;
        self.task
            .await
            .map_err(|error| format!("race proxy task failed: {error}"))?
            .map_err(|error| format!("race proxy shutdown failed: {error}"))
    }
}

async fn verify_compensated_failure(
    mcp: &GiteaMcp,
    token_client: &TokenLifecycleClient,
    failure_key: &str,
) -> Result<i64, String> {
    let conflicting = token_client
        .create(&CreateAccessToken {
            name: "bootstrap-conflict".to_string(),
            scopes: vec!["read:user".to_string()],
        })
        .await
        .map_err(|error| error.to_string())?;
    let failed = invoke_raw(
        mcp,
        "repository.bootstrap",
        bootstrap_declaration(
            "partial-failure",
            failure_key,
            "bootstrap-conflict",
            &["write:repository"],
        ),
    )
    .await?;
    if failed.is_error != Some(true) {
        return Err("scope conflict unexpectedly completed".to_string());
    }
    let failed = failed
        .structured_content
        .ok_or_else(|| "scope conflict returned no structured result".to_string())?;
    assert_compensations(
        &failed,
        &[
            "repoDelete",
            "issueDeleteLabel",
            "repoDeleteHook",
            "repoDeleteKey",
            "deleteRepoVariable",
            "deleteRepoSecret",
            "repoDeleteBranchProtection",
        ],
    )?;
    if failed["steps"]
        .as_array()
        .and_then(|steps| steps.last())
        .and_then(|step| step["operation_id"].as_str())
        != Some("userGetTokens")
    {
        return Err("scope conflict was not the terminal bootstrap step".to_string());
    }
    Ok(conflicting.id)
}

async fn verify_revoked(agent_mcp: &GiteaMcp) -> Result<(), String> {
    let revoked = invoke_raw(
        agent_mcp,
        "repository.get",
        object(json!({"owner": ORGANIZATION, "repo": "complete"}))?,
    )
    .await?;
    if revoked.is_error != Some(true)
        || revoked
            .structured_content
            .as_ref()
            .and_then(|value| value["status"].as_u64())
            != Some(401)
    {
        return Err("revoked bootstrap token remained usable".to_string());
    }
    Ok(())
}

fn bootstrap_declaration(
    repository: &str,
    deploy_key: &str,
    token_name: &str,
    token_scopes: &[&str],
) -> Map<String, Value> {
    object(json!({
        "owner": ORGANIZATION,
        "owner_kind": "organization",
        "repository": {
            "name": repository,
            "private": true,
            "description": "disposable repository bootstrap smoke"
        },
        "if_exists": "adopt",
        "settings": {
            "has_actions": true,
            "has_issues": true,
            "has_pull_requests": true
        },
        "topics": ["mcp", "bootstrap-smoke"],
        "teams": ["developers"],
        "collaborators": [{
            "username": "scope-collaborator",
            "permission": "read"
        }],
        "labels": [{
            "name": "priority/high",
            "color": "d73a4a",
            "description": "urgent"
        }],
        "hooks": [{
            "type": "gitea",
            "active": true,
            "config": {
                "content_type": "json",
                "url": format!("https://hooks.example.test/{repository}")
            },
            "events": ["push"]
        }],
        "deploy_keys": [{
            "title": "deploy",
            "key": deploy_key,
            "read_only": true
        }],
        "actions_variables": [{
            "name": "CI_MODE",
            "value": "strict",
            "description": "bootstrap smoke"
        }],
        "actions_secrets": [{
            "name": "DEPLOY_TOKEN",
            "data": "disposable-secret",
            "description": "bootstrap smoke"
        }],
        "branch_protections": [{
            "rule_name": "main",
            "enable_status_check": true,
            "status_check_contexts": ["test / test"]
        }],
        "access_token": {
            "name": token_name,
            "scopes": token_scopes
        }
    }))
    .expect("bootstrap declaration")
}

async fn bootstrap_success(mcp: &GiteaMcp, arguments: Map<String, Value>) -> Result<Value, String> {
    let result = invoke_raw(mcp, "repository.bootstrap", arguments).await?;
    if result.is_error == Some(true) {
        let summary = result
            .structured_content
            .as_ref()
            .and_then(|value| value["steps"].as_array())
            .and_then(|steps| steps.last())
            .map_or_else(
                || "no failure detail".to_string(),
                |step| {
                    format!(
                        "{}: {} (HTTP {})",
                        step["operation_id"].as_str().unwrap_or("unknown"),
                        step["detail"].as_str().unwrap_or("failed"),
                        step["status"]
                            .as_u64()
                            .map_or_else(|| "none".to_string(), |status| status.to_string())
                    )
                },
            );
        return Err(format!("bootstrap returned an error: {summary}"));
    }
    let value = result
        .structured_content
        .ok_or_else(|| "bootstrap returned no structured result".to_string())?;
    if value["complete"] != true {
        return Err("bootstrap result is not complete".to_string());
    }
    Ok(value)
}

async fn invoke_success(
    mcp: &GiteaMcp,
    name: &str,
    arguments: Map<String, Value>,
) -> Result<Value, String> {
    let result = invoke_raw(mcp, name, arguments).await?;
    if result.is_error == Some(true) {
        let status = result
            .structured_content
            .as_ref()
            .and_then(|value| value["status"].as_u64());
        return Err(format!(
            "{name} returned an error{}",
            status.map_or_else(String::new, |status| format!(" (HTTP {status})"))
        ));
    }
    result
        .structured_content
        .ok_or_else(|| format!("{name} returned no structured result"))
}

async fn invoke_raw(
    mcp: &GiteaMcp,
    name: &str,
    arguments: Map<String, Value>,
) -> Result<CallToolResult, String> {
    // A catalog operation is executed through its lane; a hand-written tool is
    // called directly. Routing here keeps every call site in this example
    // written in terms of the operation it means.
    let request = match gitea_mcp::lane_for(name) {
        Some(lane) => {
            CallToolRequestParams::new(lane.to_string()).with_arguments(Map::from_iter([
                ("operation_id".to_string(), json!(name)),
                ("arguments".to_string(), Value::Object(arguments)),
            ]))
        }
        None => CallToolRequestParams::new(name.to_string()).with_arguments(arguments),
    };
    mcp.invoke_tool(request)
        .await
        .map_err(|error| format!("{name} failed: {}", error.message))
}

fn assert_operations(result: &Value, required: &[&str]) -> Result<(), String> {
    let operations = result["steps"]
        .as_array()
        .ok_or_else(|| "bootstrap result has no steps".to_string())?;
    for required in required {
        if !operations
            .iter()
            .any(|step| step["operation_id"].as_str() == Some(required))
        {
            return Err(format!("bootstrap did not execute {required}"));
        }
    }
    Ok(())
}

fn assert_compensations(result: &Value, required: &[&str]) -> Result<(), String> {
    let compensations = result["compensations"]
        .as_array()
        .ok_or_else(|| "failed bootstrap has no compensations".to_string())?;
    for required in required {
        if !compensations
            .iter()
            .any(|item| item["operation_id"].as_str() == Some(required))
        {
            return Err(format!("failed bootstrap omitted {required} compensation"));
        }
    }
    Ok(())
}

fn object(value: Value) -> Result<Map<String, Value>, String> {
    match value {
        Value::Object(arguments) => Ok(arguments),
        _ => Err("tool arguments must be an object".to_string()),
    }
}

fn required(name: &'static str) -> Result<String, String> {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{name} is required"))
}
