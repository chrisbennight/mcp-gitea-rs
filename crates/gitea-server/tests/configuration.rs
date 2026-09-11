use std::process::Command;

#[test]
fn startup_requires_an_explicit_upstream_before_accepting_credentials() {
    let output = Command::new(env!("CARGO_BIN_EXE_mcp-gitea-rs"))
        .arg("--healthcheck")
        .env_clear()
        .env("GITEA_MCP_PORT", "0")
        .env("GITEA_MCP_SERVICE_TOKEN", "test-service-token")
        .env("GITEA_MCP_TOKEN_USERNAME", "test-user")
        .env("GITEA_MCP_TOKEN_PASSWORD", "test-password")
        .env(
            "GITEA_MCP_GATEWAY_BEARER_CURRENT",
            "0123456789abcdef0123456789abcdef",
        )
        .output()
        .expect("run server");

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(output.stderr).expect("UTF-8 error"),
        "configuration error: GITEA_MCP_UPSTREAM_URL is required\n"
    );
}
