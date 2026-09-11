//! Discovery driven through a real MCP session.
//!
//! The `ServerHandler` methods delegate to inherent helpers, and the unit tests
//! call those helpers directly. That leaves the delegation itself unmeasured:
//! pointing `read_resource` or `list_resources` at something else — or dropping
//! the instructions from `get_info` — would keep every unit test green while a
//! connected client saw no catalog at all. These tests speak the protocol over
//! an in-memory transport, so the wiring is what fails when it breaks.

use std::sync::Arc;
use std::time::Duration;

use gitea_api::{GiteaClient, TokenLifecycleClient};
use gitea_mcp::{CATALOG_INDEX_URI, GiteaMcp, SERVER_INSTRUCTIONS};
use rmcp::model::{ReadResourceRequestParams, ResourceContents};
use rmcp::service::{RunningService, ServiceExt};
use rmcp::{RoleClient, RoleServer};

/// Serve a handler over a duplex pair and return the connected client.
///
/// The upstream base URL is never resolved: discovery answers from the
/// generated catalog without an upstream call, which is the property being
/// relied on rather than an accident of the fixture.
async fn connected_session() -> (
    RunningService<RoleClient, ()>,
    RunningService<RoleServer, GiteaMcp>,
) {
    let client = Arc::new(
        GiteaClient::new("https://gitea.example.test", "t", Duration::from_secs(1))
            .expect("client"),
    );
    let token_client = Arc::new(
        TokenLifecycleClient::new(
            "https://gitea.example.test",
            "token-user",
            "token-password",
            Duration::from_secs(1),
        )
        .expect("token client"),
    );

    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    // Both halves of the handshake must be in flight at once: each `serve`
    // resolves only after `initialize` completes, so awaiting the server first
    // would wait for a client that has not been started.
    let serving = tokio::spawn(GiteaMcp::new(client, token_client).serve(server_io));
    let peer = ().serve(client_io).await.expect("client session initializes");
    let server = serving
        .await
        .expect("server task")
        .expect("server session initializes");
    (peer, server)
}

#[tokio::test]
async fn a_connected_client_is_handed_the_orientation_instructions() {
    let (peer, server) = connected_session().await;

    let instructions = peer
        .peer_info()
        .expect("peer info recorded during initialize")
        .instructions
        .clone()
        .expect("the server sends instructions");
    assert_eq!(instructions, SERVER_INSTRUCTIONS);

    peer.cancel().await.expect("client shuts down");
    server.cancel().await.expect("server shuts down");
}

#[tokio::test]
async fn the_catalog_index_is_listed_and_readable_over_the_protocol() {
    let (peer, server) = connected_session().await;

    let listed = peer.list_resources(None).await.expect("resources/list");
    let advertised = listed
        .resources
        .iter()
        .find(|resource| resource.uri == CATALOG_INDEX_URI)
        .expect("the index is advertised to a connected client");
    assert_eq!(
        advertised.mime_type.as_deref(),
        Some("text/tab-separated-values")
    );

    let read = peer
        .read_resource(ReadResourceRequestParams::new(CATALOG_INDEX_URI))
        .await
        .expect("resources/read");
    match &read.contents[0] {
        ResourceContents::TextResourceContents { text, uri, .. } => {
            assert_eq!(uri, CATALOG_INDEX_URI);
            assert!(text.starts_with("# tool"), "index header: {text:.40}");
            // A line per exposed tool, so a client that read the index can name
            // one without a further round trip.
            let named = text
                .lines()
                .filter(|line| !line.starts_with('#'))
                .any(|line| line.starts_with("repository.get\t"));
            assert!(named, "the index names generated tools");
        }
        other => panic!("unexpected contents: {other:?}"),
    }

    peer.cancel().await.expect("client shuts down");
    server.cancel().await.expect("server shuts down");
}

#[tokio::test]
async fn a_connected_session_lists_the_published_surface() {
    let (peer, server) = connected_session().await;

    let tools = peer
        .list_tools(Option::default())
        .await
        .expect("tools/list")
        .tools;
    assert!(
        tools.iter().any(|tool| tool.name == "api.read"),
        "a session lists the execution lanes"
    );
    assert!(
        !tools.iter().any(|tool| tool.name == "repository.get"),
        "catalog operations are executed through the lanes, not listed"
    );
    assert!(
        tools
            .iter()
            .any(|tool| tool.name == "repository.secret.set_from_file"),
        "a session lists the governed secret-input workflow"
    );
    assert_eq!(tools.len(), 12);

    peer.cancel().await.expect("client shuts down");
    server.cancel().await.expect("server shuts down");
}

#[tokio::test]
async fn no_published_input_schema_applies_a_combinator_at_its_root() {
    let (peer, server) = connected_session().await;

    let tools = peer
        .list_tools(Option::default())
        .await
        .expect("tools/list")
        .tools;

    // A client whose tool definitions must satisfy the Anthropic API refuses an
    // input schema that applies `oneOf`, `anyOf`, `allOf`, or `not` at its root
    // and drops that one tool while registering the rest, so the capability
    // disappears with nothing failing anywhere the server can see. Asserted
    // across the whole published surface rather than against the tool that once
    // carried it, because any schema built here can reach the same shape.
    // The same keywords nested under a property are accepted and stay usable.
    let offenders: Vec<(String, &str)> = tools
        .iter()
        .flat_map(|tool| {
            ["oneOf", "anyOf", "allOf", "not"]
                .into_iter()
                .filter(|keyword| tool.input_schema.contains_key(*keyword))
                .map(|keyword| (tool.name.to_string(), keyword))
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "a published input schema applies a root-level combinator: {offenders:?}"
    );

    peer.cancel().await.expect("client shuts down");
    server.cancel().await.expect("server shuts down");
}
