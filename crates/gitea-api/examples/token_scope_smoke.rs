use std::{env, process::ExitCode, time::Duration};

use gitea_api::{
    AccessTokenSelector, CreateAccessToken, GiteaClient, TokenLifecycleClient,
    catalog::exposed_operation,
};
use serde_json::{Map, json};

#[tokio::main]
async fn main() -> ExitCode {
    match verify().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("token lifecycle smoke failed: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn verify() -> Result<(), String> {
    let base_url = required("GITEA_SCOPE_TEST_URL")?;
    let username = required("GITEA_SCOPE_TEST_USERNAME")?;
    let password = required("GITEA_SCOPE_TEST_PASSWORD")?;
    let token_client =
        TokenLifecycleClient::new(&base_url, &username, &password, Duration::from_secs(10))
            .map_err(|error| error.to_string())?;
    let created = token_client
        .create(&CreateAccessToken {
            name: "mcp-gitea-rs-scope-test".to_string(),
            scopes: vec!["read:user".to_string()],
        })
        .await
        .map_err(|error| error.to_string())?;
    let scoped_client = GiteaClient::new(&base_url, &created.token, Duration::from_secs(10))
        .map_err(|error| error.to_string())?;

    let verification = verify_scope(&scoped_client).await;
    token_client
        .revoke(&AccessTokenSelector::Id(created.id))
        .await
        .map_err(|error| format!("cleanup failed: {error}"))?;
    verification?;

    let after_revoke = scoped_client
        .execute_operation(operation("user.get_current")?, Map::new())
        .await
        .map_err(|error| error.to_string())?;
    if after_revoke.status != 401 {
        return Err(format!(
            "revoked token returned HTTP {} instead of 401",
            after_revoke.status
        ));
    }
    Ok(())
}

async fn verify_scope(client: &GiteaClient) -> Result<(), String> {
    let allowed = client
        .execute_operation(operation("user.get_current")?, Map::new())
        .await
        .map_err(|error| error.to_string())?;
    if !allowed.success {
        return Err(format!(
            "read:user operation returned HTTP {}",
            allowed.status
        ));
    }

    let disallowed = client
        .execute_operation(
            operation("repository.create_for_current_user")?,
            Map::from_iter([(
                "body".to_string(),
                json!({"name": "scope-must-not-create-this-repository"}),
            )]),
        )
        .await
        .map_err(|error| error.to_string())?;
    if disallowed.status != 403 {
        return Err(format!(
            "write:repository operation returned HTTP {} instead of 403",
            disallowed.status
        ));
    }
    Ok(())
}

fn operation(name: &str) -> Result<&'static gitea_api::catalog::OperationSpec, String> {
    exposed_operation(name).ok_or_else(|| format!("catalog operation {name} is missing"))
}

fn required(name: &'static str) -> Result<String, String> {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{name} is required"))
}
