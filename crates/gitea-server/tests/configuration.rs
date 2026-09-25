use std::process::Command;

#[test]
fn ingress_configuration_rejects_invalid_limits_and_origins() {
    for (name, value) in [
        ("GITEA_MCP_BODY_TIMEOUT_SECONDS", "0"),
        ("GITEA_MCP_BODY_TIMEOUT_SECONDS", "301"),
        ("GITEA_MCP_ALLOWED_ORIGINS", "*"),
        (
            "GITEA_MCP_ALLOWED_ORIGINS",
            "https://user:synthetic-private-value@example.test",
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_mcp-gitea-rs"))
            .arg("--healthcheck")
            .env_clear()
            .env("GITEA_MCP_UPSTREAM_URL", "http://127.0.0.1:9")
            .env("GITEA_MCP_SERVICE_TOKEN", "test-service-token")
            .env(
                "GITEA_MCP_GATEWAY_BEARER_CURRENT",
                "0123456789abcdef0123456789abcdef",
            )
            .env(name, value)
            .output()
            .expect("server");
        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(name));
        assert!(!stderr.contains("synthetic-private-value"));
    }
}

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

#[test]
fn incomplete_token_credentials_fail_without_disclosing_values() {
    for name in ["GITEA_MCP_TOKEN_USERNAME", "GITEA_MCP_TOKEN_PASSWORD"] {
        let output = Command::new(env!("CARGO_BIN_EXE_mcp-gitea-rs"))
            .arg("--healthcheck")
            .env_clear()
            .env("GITEA_MCP_UPSTREAM_URL", "http://127.0.0.1:9")
            .env("GITEA_MCP_SERVICE_TOKEN", "test-service-token")
            .env(
                "GITEA_MCP_GATEWAY_BEARER_CURRENT",
                "0123456789abcdef0123456789abcdef",
            )
            .env(name, "must-not-appear-in-output")
            .output()
            .expect("server");
        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8(output.stderr).expect("UTF-8");
        assert!(stderr.contains("configure both credentials or omit both"));
        assert!(!stderr.contains("must-not-appear-in-output"));
    }
}

#[test]
fn pat_only_configuration_reaches_healthcheck() {
    // Port zero cannot identify an existing listener, so no external service is contacted.
    let output = Command::new(env!("CARGO_BIN_EXE_mcp-gitea-rs"))
        .arg("--healthcheck")
        .env_clear()
        .env("GITEA_MCP_UPSTREAM_URL", "http://127.0.0.1:9")
        .env("GITEA_MCP_SERVICE_TOKEN", "test-service-token")
        .env(
            "GITEA_MCP_GATEWAY_BEARER_CURRENT",
            "0123456789abcdef0123456789abcdef",
        )
        .env("GITEA_MCP_HOST", "127.0.0.1")
        .env("GITEA_MCP_PORT", "0")
        .output()
        .expect("server");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        !String::from_utf8(output.stderr)
            .expect("UTF-8")
            .contains("configuration error")
    );
}
