use std::{borrow::Cow, collections::HashSet, sync::Arc};

use gitea_api::{
    AccessTokenCreated, AccessTokenMetadata, ApiError, CallOutcome, CreateAccessToken, GiteaClient,
    OperationResponse, TokenLifecycleClient, catalog::exposed_operation_by_id,
    validate_create_access_token, validate_path_segment,
};
use rmcp::model::{Meta, Tool, ToolAnnotations};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

mod extensions;

pub const TOOL_NAME: &str = "repository.bootstrap";
const MAX_ITEMS: usize = 100;
const MAX_TOKEN_SEARCH_PAGES: u32 = 20;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Arguments {
    owner: String,
    owner_kind: OwnerKind,
    repository: Map<String, Value>,
    #[serde(default)]
    if_exists: IfExists,
    settings: Option<Map<String, Value>>,
    topics: Option<Vec<String>>,
    #[serde(default)]
    teams: Vec<String>,
    #[serde(default)]
    collaborators: Vec<Collaborator>,
    #[serde(default)]
    branch_protections: Vec<Map<String, Value>>,
    #[serde(flatten)]
    extensions: extensions::Arguments,
    access_token: Option<CreateAccessToken>,
}

impl Arguments {
    pub fn requires_token_administration(&self) -> bool {
        self.access_token.is_some()
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OwnerKind {
    CurrentUser,
    Organization,
    User,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum IfExists {
    #[default]
    Fail,
    Adopt,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Collaborator {
    username: String,
    permission: String,
}

#[derive(Serialize)]
pub struct BootstrapResult {
    complete: bool,
    repository_created: bool,
    steps: Vec<Step>,
    compensations: Vec<Compensation>,
    access_token: Option<AccessTokenCreated>,
}

#[derive(Serialize)]
struct Step {
    name: String,
    operation_id: String,
    outcome: &'static str,
    status: Option<u16>,
    detail: String,
}

#[derive(Serialize)]
struct Compensation {
    tool: String,
    operation_id: String,
    arguments: Map<String, Value>,
    reason: String,
}

impl BootstrapResult {
    fn new() -> Self {
        Self {
            complete: false,
            repository_created: false,
            steps: Vec::new(),
            compensations: Vec::new(),
            access_token: None,
        }
    }

    fn failed_step(
        &mut self,
        name: impl Into<String>,
        operation_id: impl Into<String>,
        status: Option<u16>,
        detail: impl Into<String>,
    ) {
        self.steps.push(Step {
            name: name.into(),
            operation_id: operation_id.into(),
            outcome: "failed",
            status,
            detail: detail.into(),
        });
    }

    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.complete
    }
}

pub fn tool() -> Tool {
    let mut tool = Tool::new(
        Cow::Borrowed(TOOL_NAME),
        Cow::Borrowed(
            "Validate, create or adopt, and converge a repository; return outcomes and \
             compensation.",
        ),
        Arc::new(input_schema()),
    )
    .with_annotations(
        ToolAnnotations::new()
            .read_only(false)
            .destructive(true)
            .idempotent(false)
            .open_world(true),
    );
    tool.output_schema = Some(Arc::new(output_schema()));
    tool.meta = Some(Meta(Map::from_iter([
        (
            "org.cacahuate/operationId".to_string(),
            Value::String("repositoryBootstrap".to_string()),
        ),
        (
            "org.cacahuate/administrative".to_string(),
            Value::Bool(true),
        ),
        (
            "org.cacahuate/risk".to_string(),
            Value::String("destructive".to_string()),
        ),
        (
            "org.cacahuate/sensitiveResult".to_string(),
            Value::Bool(true),
        ),
        (
            "org.cacahuate/sensitiveInput".to_string(),
            Value::Bool(true),
        ),
    ])));
    tool
}

fn input_schema() -> Map<String, Value> {
    let mut properties = json!({
        "owner": {"type": "string", "minLength": 1, "maxLength": 255},
        "owner_kind": {
            "type": "string",
            "enum": ["current_user", "organization", "user"]
        },
        "repository": operation_property_schema("createCurrentUserRepo", "body"),
        "if_exists": {
            "type": "string",
            "enum": ["fail", "adopt"],
            "default": "fail"
        },
        "settings": repository_settings_schema(),
        "topics": string_array_schema(50),
        "teams": string_array_schema(255),
        "collaborators": collaborator_array_schema(),
        "branch_protections": {
            "type": "array",
            "maxItems": MAX_ITEMS,
            "items": branch_protection_schema()
        },
        "access_token": access_token_schema()
    })
    .as_object()
    .expect("bootstrap properties")
    .clone();
    properties.extend(extensions::schema_properties());
    object_schema(
        Value::Object(properties),
        &["owner", "owner_kind", "repository"],
    )
}

fn string_array_schema(max_length: usize) -> Value {
    json!({
        "type": "array",
        "maxItems": MAX_ITEMS,
        "uniqueItems": true,
        "items": {"type": "string", "minLength": 1, "maxLength": max_length}
    })
}

fn collaborator_array_schema() -> Value {
    json!({
        "type": "array",
        "maxItems": MAX_ITEMS,
        "items": {
            "type": "object",
            "properties": {
                "username": {"type": "string", "minLength": 1, "maxLength": 255},
                "permission": {"type": "string", "enum": ["read", "write", "admin"]}
            },
            "required": ["username", "permission"],
            "additionalProperties": false
        }
    })
}

fn repository_settings_schema() -> Value {
    let mut schema = operation_property_schema("repoEdit", "body");
    schema
        .get_mut("properties")
        .and_then(Value::as_object_mut)
        .expect("repository settings schema properties")
        .remove("name");
    schema
}

fn branch_protection_schema() -> Value {
    let mut schema = operation_property_schema("repoCreateBranchProtection", "body");
    schema["required"] = json!(["rule_name"]);
    schema["properties"]
        .as_object_mut()
        .expect("branch protection schema properties")
        .remove("branch_name");
    schema
}

fn access_token_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "minLength": 1,
                "maxLength": 255,
                "pattern": "^(?![+-]?[0-9]+$).+$"
            },
            "scopes": {
                "type": "array",
                "minItems": 1,
                "maxItems": 9,
                "uniqueItems": true,
                "items": {"type": "string", "enum": super::token_scopes()}
            }
        },
        "required": ["name", "scopes"],
        "additionalProperties": false
    })
}

pub fn parse(arguments: Option<Map<String, Value>>) -> Result<Arguments, &'static str> {
    serde_json::from_value(Value::Object(arguments.unwrap_or_default()))
        .map_err(|_| "arguments do not match the repository bootstrap schema")
}

pub fn validate_input(arguments: &Map<String, Value>) -> Result<(), &'static str> {
    let schema = Value::Object(tool().input_schema.as_ref().clone());
    let validator = jsonschema::draft4::options()
        .build(&schema)
        .map_err(|_| "repository bootstrap schema is invalid")?;
    validator
        .validate(&Value::Object(arguments.clone()))
        .map_err(|_| "arguments do not match the repository bootstrap schema")
}

pub fn validate(arguments: &Arguments) -> Result<(), &'static str> {
    if arguments.owner.is_empty() || arguments.owner.chars().count() > 255 {
        return Err("owner is invalid");
    }
    for length in [
        arguments.topics.as_ref().map_or(0, Vec::len),
        arguments.teams.len(),
        arguments.collaborators.len(),
        arguments.branch_protections.len(),
        arguments.extensions.max_collection_len(),
    ] {
        if length > MAX_ITEMS {
            return Err("repository bootstrap collection exceeds its bound");
        }
    }
    if !arguments.teams.is_empty() && !matches!(arguments.owner_kind, OwnerKind::Organization) {
        return Err("teams require an organization repository");
    }
    let repository_name = repository_name(arguments)?;
    validate_path("owner", &arguments.owner)?;
    validate_path("repo", repository_name)?;
    validate_operation(
        create_operation_id(&arguments.owner_kind),
        create_arguments(arguments),
    )?;
    if matches!(arguments.owner_kind, OwnerKind::CurrentUser) {
        validate_operation("userGetCurrent", Map::new())?;
    }
    validate_operation(
        "repoGet",
        repository_arguments(&arguments.owner, repository_name),
    )?;
    if let Some(settings) = &arguments.settings {
        validate_operation(
            "repoEdit",
            repository_body_arguments(&arguments.owner, repository_name, settings.clone()),
        )?;
    }
    if let Some(topics) = &arguments.topics {
        validate_operation(
            "repoUpdateTopics",
            repository_body_arguments(
                &arguments.owner,
                repository_name,
                Map::from_iter([("topics".to_string(), json!(topics))]),
            ),
        )?;
    }
    for team in &arguments.teams {
        validate_path("team", team)?;
        let operation_arguments = Map::from_iter([
            ("owner".to_string(), json!(arguments.owner)),
            ("repo".to_string(), json!(repository_name)),
            ("team".to_string(), json!(team)),
        ]);
        for operation_id in ["repoCheckTeam", "repoAddTeam", "repoDeleteTeam"] {
            validate_operation(operation_id, operation_arguments.clone())?;
        }
    }
    let collaborators = arguments
        .collaborators
        .iter()
        .map(|collaborator| collaborator.username.as_str());
    validate_unique_targets(collaborators)?;
    for collaborator in &arguments.collaborators {
        validate_path("collaborator", &collaborator.username)?;
        validate_operation(
            "repoAddCollaborator",
            repository_named_body_arguments(
                &arguments.owner,
                repository_name,
                "collaborator",
                &collaborator.username,
                Map::from_iter([("permission".to_string(), json!(collaborator.permission))]),
            ),
        )?;
    }
    let branches = arguments
        .branch_protections
        .iter()
        .filter_map(|protection| protection.get("rule_name").and_then(Value::as_str));
    validate_unique_targets(branches)?;
    validate_branch_protections(arguments, repository_name)?;
    extensions::validate(&arguments.extensions, &arguments.owner, repository_name)?;
    if let Some(token) = &arguments.access_token {
        validate_create_access_token(token).map_err(|_| "access token declaration is invalid")?;
    }
    Ok(())
}

fn validate_branch_protections(
    arguments: &Arguments,
    repository_name: &str,
) -> Result<(), &'static str> {
    for protection in &arguments.branch_protections {
        let name = protection
            .get("rule_name")
            .and_then(Value::as_str)
            .ok_or("branch protection rule_name is required")?;
        validate_path("name", name)?;
        validate_operation(
            "repoCreateBranchProtection",
            repository_body_arguments(&arguments.owner, repository_name, protection.clone()),
        )?;
        let mut edit_body = protection.clone();
        edit_body.remove("rule_name");
        validate_operation(
            "repoEditBranchProtection",
            repository_named_body_arguments(
                &arguments.owner,
                repository_name,
                "name",
                name,
                edit_body,
            ),
        )?;
    }
    Ok(())
}

pub async fn execute(
    client: &GiteaClient,
    token_client: Option<&TokenLifecycleClient>,
    arguments: Arguments,
) -> BootstrapResult {
    let mut result = BootstrapResult::new();
    if arguments.requires_token_administration() && token_client.is_none() {
        result.failed_step(
            "access_token",
            "access_token.create",
            None,
            "Token administration is not configured; no upstream request was sent",
        );
        return result;
    }
    let repository_name = repository_name(&arguments)
        .expect("validated repository declaration")
        .to_string();
    if !verify_owner(client, &arguments, &mut result).await
        || !ensure_repository(client, &arguments, &repository_name, &mut result).await
        || !apply_repository_state(client, &arguments, &repository_name, &mut result).await
    {
        return result;
    }
    if let Some(token_client) = token_client
        && !apply_access_token(token_client, &arguments, &mut result).await
    {
        return result;
    }
    result.complete = true;
    result.compensations.clear();
    result
}

async fn verify_owner(
    client: &GiteaClient,
    arguments: &Arguments,
    result: &mut BootstrapResult,
) -> bool {
    if !matches!(arguments.owner_kind, OwnerKind::CurrentUser) {
        return true;
    }
    match invoke(client, "userGetCurrent", Map::new()).await {
        Ok(response)
            if response.success
                && response.data.get("login").and_then(Value::as_str)
                    == Some(arguments.owner.as_str()) =>
        {
            push_step(
                result,
                "owner.verify",
                "userGetCurrent",
                "unchanged",
                Some(response.status),
                "service identity matches owner",
            );
            true
        }
        Ok(response) => {
            let detail = if response.success {
                "service identity does not match owner".to_string()
            } else {
                refusal_detail(&response, "Gitea rejected service-identity lookup")
            };
            result.failed_step(
                "owner.verify",
                "userGetCurrent",
                Some(response.status),
                detail,
            );
            false
        }
        Err(error) => {
            let (status, detail) = failure_detail(&error);
            result.failed_step("owner.verify", "userGetCurrent", status, detail);
            false
        }
    }
}

async fn ensure_repository(
    client: &GiteaClient,
    arguments: &Arguments,
    repository_name: &str,
    result: &mut BootstrapResult,
) -> bool {
    let lookup_arguments = repository_arguments(&arguments.owner, repository_name);
    match invoke(client, "repoGet", lookup_arguments.clone()).await {
        Ok(response) if response.success && matches!(arguments.if_exists, IfExists::Adopt) => {
            push_step(
                result,
                "repository.adopt",
                "repoGet",
                "unchanged",
                Some(response.status),
                "repository exists and was adopted",
            );
            true
        }
        Ok(response) if response.success => {
            result.failed_step(
                "repository.lookup",
                "repoGet",
                Some(response.status),
                "repository exists; adoption forbidden",
            );
            false
        }
        Ok(response) if response.status == 404 => {
            create_repository(client, arguments, lookup_arguments, result).await
        }
        Ok(response) => {
            result.failed_step(
                "repository.lookup",
                "repoGet",
                Some(response.status),
                refusal_detail(&response, "Gitea rejected repository lookup"),
            );
            false
        }
        Err(error) => {
            let (status, detail) = failure_detail(&error);
            result.failed_step("repository.lookup", "repoGet", status, detail);
            false
        }
    }
}

async fn create_repository(
    client: &GiteaClient,
    arguments: &Arguments,
    lookup_arguments: Map<String, Value>,
    result: &mut BootstrapResult,
) -> bool {
    let create_id = create_operation_id(&arguments.owner_kind);
    match invoke(client, create_id, create_arguments(arguments)).await {
        Ok(response) if response.success => {
            result.repository_created = true;
            push_step(
                result,
                "repository.create",
                create_id,
                "applied",
                Some(response.status),
                "repository created",
            );
            result.compensations.push(Compensation {
                tool: "repository.delete".to_string(),
                operation_id: "repoDelete".to_string(),
                arguments: lookup_arguments,
                reason: "delete repository created before failure".to_string(),
            });
            true
        }
        Ok(response)
            if response.status == 409 && matches!(arguments.if_exists, IfExists::Adopt) =>
        {
            adopt_after_conflict(client, lookup_arguments, result).await
        }
        Ok(response) => {
            result.failed_step(
                "repository.create",
                create_id,
                Some(response.status),
                refusal_detail(&response, "Gitea rejected repository creation"),
            );
            false
        }
        Err(error) => {
            let (status, detail) = failure_detail(&error);
            result.failed_step("repository.create", create_id, status, detail);
            false
        }
    }
}

async fn adopt_after_conflict(
    client: &GiteaClient,
    lookup_arguments: Map<String, Value>,
    result: &mut BootstrapResult,
) -> bool {
    match invoke(client, "repoGet", lookup_arguments).await {
        Ok(current) if current.success => {
            push_step(
                result,
                "repository.adopt",
                "repoGet",
                "unchanged",
                Some(current.status),
                "repository race adopted",
            );
            true
        }
        Ok(current) => {
            result.failed_step(
                "repository.adopt",
                "repoGet",
                Some(current.status),
                refusal_detail(&current, "conflicted repository is unreadable"),
            );
            false
        }
        Err(error) => {
            let (status, detail) = failure_detail(&error);
            result.failed_step("repository.adopt", "repoGet", status, detail);
            false
        }
    }
}

async fn apply_repository_state(
    client: &GiteaClient,
    arguments: &Arguments,
    repository_name: &str,
    result: &mut BootstrapResult,
) -> bool {
    apply_settings_and_topics(client, arguments, repository_name, result).await
        && apply_teams(client, arguments, repository_name, result).await
        && apply_collaborators(client, arguments, repository_name, result).await
        && extensions::apply(
            client,
            &arguments.extensions,
            &arguments.owner,
            repository_name,
            result,
        )
        .await
        && apply_branch_protections(client, arguments, repository_name, result).await
        && apply_deferred_settings(client, arguments, repository_name, result).await
}

async fn apply_settings_and_topics(
    client: &GiteaClient,
    arguments: &Arguments,
    repository_name: &str,
    result: &mut BootstrapResult,
) -> bool {
    if let Some(settings) = &arguments.settings
        && !archives(arguments)
        && !apply_operation(
            client,
            result,
            "repository.settings",
            "repoEdit",
            repository_body_arguments(&arguments.owner, repository_name, settings.clone()),
        )
        .await
    {
        return false;
    }
    match &arguments.topics {
        None => true,
        Some(topics) => {
            apply_operation(
                client,
                result,
                "repository.topics",
                "repoUpdateTopics",
                repository_body_arguments(
                    &arguments.owner,
                    repository_name,
                    Map::from_iter([("topics".to_string(), json!(topics))]),
                ),
            )
            .await
        }
    }
}

async fn apply_deferred_settings(
    client: &GiteaClient,
    arguments: &Arguments,
    repository_name: &str,
    result: &mut BootstrapResult,
) -> bool {
    if !archives(arguments) {
        return true;
    }
    apply_operation(
        client,
        result,
        "repository.archive",
        "repoEdit",
        repository_body_arguments(
            &arguments.owner,
            repository_name,
            arguments.settings.clone().expect("archive settings"),
        ),
    )
    .await
}

fn archives(arguments: &Arguments) -> bool {
    arguments
        .settings
        .as_ref()
        .is_some_and(|settings| settings.get("archived") == Some(&Value::Bool(true)))
}

async fn apply_teams(
    client: &GiteaClient,
    arguments: &Arguments,
    repository_name: &str,
    result: &mut BootstrapResult,
) -> bool {
    for team in &arguments.teams {
        let operation_arguments = Map::from_iter([
            ("owner".to_string(), json!(arguments.owner)),
            ("repo".to_string(), json!(repository_name)),
            ("team".to_string(), json!(team)),
        ]);
        if !apply_team(client, result, team, operation_arguments).await {
            return false;
        }
    }
    true
}

async fn apply_team(
    client: &GiteaClient,
    result: &mut BootstrapResult,
    team: &str,
    arguments: Map<String, Value>,
) -> bool {
    match invoke(client, "repoCheckTeam", arguments.clone()).await {
        Ok(response) if response.success => {
            push_step(
                result,
                format!("team.{team}"),
                "repoCheckTeam",
                "unchanged",
                Some(response.status),
                "team already has repository access",
            );
            true
        }
        Ok(response) if response.status == 404 => add_team(client, result, team, arguments).await,
        Ok(response) => {
            result.failed_step(
                format!("team.{team}"),
                "repoCheckTeam",
                Some(response.status),
                refusal_detail(&response, "Gitea rejected team lookup"),
            );
            false
        }
        Err(error) => {
            let (status, detail) = failure_detail(&error);
            result.failed_step(format!("team.{team}"), "repoCheckTeam", status, detail);
            false
        }
    }
}

async fn add_team(
    client: &GiteaClient,
    result: &mut BootstrapResult,
    team: &str,
    arguments: Map<String, Value>,
) -> bool {
    match invoke(client, "repoAddTeam", arguments.clone()).await {
        Ok(response) if response.success => {
            push_step(
                result,
                format!("team.{team}"),
                "repoAddTeam",
                "applied",
                Some(response.status),
                "team added",
            );
            result.compensations.push(Compensation {
                tool: "repository.delete_team".to_string(),
                operation_id: "repoDeleteTeam".to_string(),
                arguments,
                reason: "remove team added before failure".to_string(),
            });
            true
        }
        Ok(response) if response.status == 422 => {
            reconcile_team_add(client, result, team, arguments).await
        }
        Ok(response) => {
            result.failed_step(
                format!("team.{team}"),
                "repoAddTeam",
                Some(response.status),
                refusal_detail(&response, "Gitea rejected team add"),
            );
            false
        }
        Err(error) => {
            let (status, detail) = failure_detail(&error);
            result.failed_step(format!("team.{team}"), "repoAddTeam", status, detail);
            false
        }
    }
}

async fn reconcile_team_add(
    client: &GiteaClient,
    result: &mut BootstrapResult,
    team: &str,
    arguments: Map<String, Value>,
) -> bool {
    match invoke(client, "repoCheckTeam", arguments).await {
        Ok(response) if response.success => {
            push_step(
                result,
                format!("team.{team}"),
                "repoCheckTeam",
                "unchanged",
                Some(response.status),
                "concurrent team add was adopted",
            );
            true
        }
        Ok(response) => {
            result.failed_step(
                format!("team.{team}"),
                "repoCheckTeam",
                Some(response.status),
                refusal_detail(&response, "conflicted team add is not visible"),
            );
            false
        }
        Err(error) => {
            let (status, detail) = failure_detail(&error);
            result.failed_step(format!("team.{team}"), "repoCheckTeam", status, detail);
            false
        }
    }
}

async fn apply_collaborators(
    client: &GiteaClient,
    arguments: &Arguments,
    repository_name: &str,
    result: &mut BootstrapResult,
) -> bool {
    for collaborator in &arguments.collaborators {
        let operation_arguments = repository_named_body_arguments(
            &arguments.owner,
            repository_name,
            "collaborator",
            &collaborator.username,
            Map::from_iter([("permission".to_string(), json!(collaborator.permission))]),
        );
        if !apply_operation(
            client,
            result,
            format!("collaborator.{}", collaborator.username),
            "repoAddCollaborator",
            operation_arguments,
        )
        .await
        {
            return false;
        }
    }
    true
}

async fn apply_branch_protections(
    client: &GiteaClient,
    arguments: &Arguments,
    repository_name: &str,
    result: &mut BootstrapResult,
) -> bool {
    for protection in &arguments.branch_protections {
        let name = protection
            .get("rule_name")
            .and_then(Value::as_str)
            .expect("validated branch protection");
        if !apply_branch_protection(
            client,
            result,
            &arguments.owner,
            repository_name,
            name,
            protection,
        )
        .await
        {
            return false;
        }
    }
    true
}

async fn apply_access_token(
    token_client: &TokenLifecycleClient,
    arguments: &Arguments,
    result: &mut BootstrapResult,
) -> bool {
    let Some(token) = &arguments.access_token else {
        return true;
    };
    match find_access_token(token_client, &token.name).await {
        Ok(Some(existing)) if scopes_match(&existing.scopes, &token.scopes) => {
            push_step(
                result,
                format!("access_token.{}", token.name),
                "userGetTokens",
                "unchanged",
                Some(200),
                "matching token exists; no value",
            );
            true
        }
        Ok(Some(_)) => {
            result.failed_step(
                format!("access_token.{}", token.name),
                "userGetTokens",
                Some(200),
                "named token has different scopes; revoke before retrying",
            );
            false
        }
        Ok(None) => create_access_token(token_client, token, result).await,
        Err(error) => {
            let (status, detail) = failure_detail(&error);
            result.failed_step(
                format!("access_token.{}", token.name),
                "userGetTokens",
                status,
                detail,
            );
            false
        }
    }
}

async fn find_access_token(
    token_client: &TokenLifecycleClient,
    name: &str,
) -> Result<Option<AccessTokenMetadata>, ApiError> {
    for page in 1..=MAX_TOKEN_SEARCH_PAGES {
        let tokens = token_client.list(Some(page), Some(100)).await?;
        // A server may cap a requested page below its requested limit. Only an
        // empty page establishes exhaustion when continuation data is absent.
        if tokens.is_empty() {
            return Ok(None);
        }
        if let Some(token) = tokens.into_iter().find(|candidate| candidate.name == name) {
            return Ok(Some(token));
        }
    }
    // The list reached Gitea and answered 200; what was exceeded is this
    // workflow's own search bound. Reporting it as never sent would be false
    // about the call.
    Err(
        ApiError::InvalidAccessTokenInput("token inventory exceeds the bootstrap search bound")
            .delivered_with(200),
    )
}

fn scopes_match(existing: &[String], requested: &[String]) -> bool {
    let mut existing = existing.to_vec();
    let mut requested = requested.to_vec();
    existing.sort_unstable();
    requested.sort_unstable();
    existing == requested
}

async fn create_access_token(
    token_client: &TokenLifecycleClient,
    token: &CreateAccessToken,
    result: &mut BootstrapResult,
) -> bool {
    match token_client.create(token).await {
        Ok(created) => {
            push_step(
                result,
                format!("access_token.{}", token.name),
                "userCreateToken",
                "applied",
                Some(201),
                "access token created; value returned once",
            );
            result.compensations.push(Compensation {
                tool: "access_token.revoke".to_string(),
                operation_id: "userDeleteAccessToken".to_string(),
                arguments: Map::from_iter([("id".to_string(), json!(created.id))]),
                reason: "revoke token created before failure".to_string(),
            });
            result.access_token = Some(created);
            true
        }
        Err(ApiError::Upstream { status, message }) => {
            result.failed_step(
                format!("access_token.{}", token.name),
                "userCreateToken",
                Some(status),
                message,
            );
            false
        }
        Err(error) => {
            let (status, detail) = failure_detail(&error);
            result.failed_step(
                format!("access_token.{}", token.name),
                "userCreateToken",
                status,
                detail,
            );
            false
        }
    }
}

async fn apply_branch_protection(
    client: &GiteaClient,
    result: &mut BootstrapResult,
    owner: &str,
    repository: &str,
    name: &str,
    protection: &Map<String, Value>,
) -> bool {
    let lookup_arguments = Map::from_iter([
        ("owner".to_string(), json!(owner)),
        ("repo".to_string(), json!(repository)),
        ("name".to_string(), json!(name)),
    ]);
    match invoke(client, "repoGetBranchProtection", lookup_arguments.clone()).await {
        Ok(response) if response.success => {
            let mut body = protection.clone();
            body.remove("rule_name");
            apply_operation(
                client,
                result,
                format!("branch_protection.{name}"),
                "repoEditBranchProtection",
                repository_named_body_arguments(owner, repository, "name", name, body),
            )
            .await
        }
        Ok(response) if response.status == 404 => {
            let create_arguments = repository_body_arguments(owner, repository, protection.clone());
            match invoke(client, "repoCreateBranchProtection", create_arguments).await {
                Ok(created) if created.success => {
                    push_step(
                        result,
                        format!("branch_protection.{name}"),
                        "repoCreateBranchProtection",
                        "applied",
                        Some(created.status),
                        "branch protection created",
                    );
                    result.compensations.push(Compensation {
                        tool: "repository.delete_branch_protection".to_string(),
                        operation_id: "repoDeleteBranchProtection".to_string(),
                        arguments: lookup_arguments,
                        reason: "delete created branch protection".to_string(),
                    });
                    true
                }
                Ok(created) if matches!(created.status, 409 | 422) => {
                    reconcile_conflicted_branch_protection(
                        client,
                        result,
                        owner,
                        repository,
                        name,
                        protection,
                        lookup_arguments,
                    )
                    .await
                }
                Ok(created) => {
                    result.failed_step(
                        format!("branch_protection.{name}"),
                        "repoCreateBranchProtection",
                        Some(created.status),
                        refusal_detail(&created, "Gitea rejected branch-protection creation"),
                    );
                    false
                }
                Err(error) => {
                    let (status, detail) = failure_detail(&error);
                    result.failed_step(
                        format!("branch_protection.{name}"),
                        "repoCreateBranchProtection",
                        status,
                        detail,
                    );
                    false
                }
            }
        }
        Ok(response) => {
            result.failed_step(
                format!("branch_protection.{name}"),
                "repoGetBranchProtection",
                Some(response.status),
                refusal_detail(&response, "Gitea rejected branch-protection lookup"),
            );
            false
        }
        Err(error) => {
            let (status, detail) = failure_detail(&error);
            result.failed_step(
                format!("branch_protection.{name}"),
                "repoGetBranchProtection",
                status,
                detail,
            );
            false
        }
    }
}

async fn reconcile_conflicted_branch_protection(
    client: &GiteaClient,
    result: &mut BootstrapResult,
    owner: &str,
    repository: &str,
    name: &str,
    protection: &Map<String, Value>,
    lookup_arguments: Map<String, Value>,
) -> bool {
    match invoke(client, "repoGetBranchProtection", lookup_arguments).await {
        Ok(reread) if reread.success => {
            let mut body = protection.clone();
            body.remove("rule_name");
            apply_operation(
                client,
                result,
                format!("branch_protection.{name}"),
                "repoEditBranchProtection",
                repository_named_body_arguments(owner, repository, "name", name, body),
            )
            .await
        }
        Ok(reread) => {
            result.failed_step(
                format!("branch_protection.{name}"),
                "repoGetBranchProtection",
                Some(reread.status),
                refusal_detail(&reread, "conflicted protection is unreadable"),
            );
            false
        }
        Err(error) => {
            let (status, detail) = failure_detail(&error);
            result.failed_step(
                format!("branch_protection.{name}"),
                "repoGetBranchProtection",
                status,
                detail,
            );
            false
        }
    }
}

async fn apply_operation(
    client: &GiteaClient,
    result: &mut BootstrapResult,
    name: impl Into<String>,
    operation_id: &'static str,
    arguments: Map<String, Value>,
) -> bool {
    let name = name.into();
    match invoke(client, operation_id, arguments).await {
        Ok(response) if response.success => {
            push_step(
                result,
                name,
                operation_id,
                "applied",
                Some(response.status),
                "Gitea accepted the declared state",
            );
            true
        }
        Ok(response) => {
            result.failed_step(
                name,
                operation_id,
                Some(response.status),
                refusal_detail(&response, "Gitea rejected the declared state"),
            );
            false
        }
        Err(error) => {
            let (status, detail) = failure_detail(&error);
            result.failed_step(name, operation_id, status, detail);
            false
        }
    }
}

/// Which step failed, and what Gitea objected to.
///
/// `context` names the step in this workflow's terms; the generated lane
/// supplies the upstream reason, already normalized and redacted, in
/// `data.message`. Reporting only the context leaves a caller knowing the
/// workflow stopped but not what to change, and reporting only the reason
/// loses which of several similar steps produced it.
pub(super) fn refusal_detail(response: &OperationResponse, context: &str) -> String {
    response
        .data
        .get("message")
        .and_then(Value::as_str)
        .filter(|message| !message.trim().is_empty())
        .map_or_else(
            || context.to_string(),
            |message| format!("{context}: {message}"),
        )
}

/// Record how far a failed step got, alongside why it failed.
///
/// A step whose call reached Gitea is not the same as one that never left, and
/// a bootstrap that reports both as a bare failure gives its caller no way to
/// tell a safe re-run from one that would act twice.
pub(super) fn failure_detail(error: &ApiError) -> (Option<u16>, String) {
    match error.call_outcome() {
        CallOutcome::Completed { status } => (Some(status), error.to_string()),
        CallOutcome::Unknown => (
            None,
            format!("the request was sent and the outcome is unknown: {error}"),
        ),
        CallOutcome::NotSent => (None, format!("the request was never sent: {error}")),
    }
}

async fn invoke(
    client: &GiteaClient,
    operation_id: &str,
    arguments: Map<String, Value>,
) -> std::result::Result<OperationResponse, ApiError> {
    let operation = exposed_operation_by_id(operation_id).ok_or(ApiError::InvalidOperation)?;
    client.execute_operation(operation, arguments).await
}

fn push_step(
    result: &mut BootstrapResult,
    name: impl Into<String>,
    operation_id: impl Into<String>,
    outcome: &'static str,
    status: Option<u16>,
    detail: impl Into<String>,
) {
    result.steps.push(Step {
        name: name.into(),
        operation_id: operation_id.into(),
        outcome,
        status,
        detail: detail.into(),
    });
}

fn validate_operation(
    operation_id: &str,
    arguments: Map<String, Value>,
) -> Result<(), &'static str> {
    let operation =
        exposed_operation_by_id(operation_id).ok_or("bootstrap operation is missing")?;
    let schema = Value::Object(operation.input_schema.clone());
    let validator = jsonschema::draft4::options()
        .build(&schema)
        .map_err(|_| "bootstrap operation schema is invalid")?;
    validator
        .validate(&Value::Object(arguments))
        .map_err(|_| "bootstrap declaration does not match a Gitea operation")
}

fn validate_path(name: &str, value: &str) -> Result<(), &'static str> {
    validate_path_segment(name, value).map_err(|_| "bootstrap contains an invalid path segment")
}

fn validate_unique_targets<'a>(
    targets: impl IntoIterator<Item = &'a str>,
) -> Result<(), &'static str> {
    let mut seen = HashSet::new();
    targets
        .into_iter()
        .all(|target| seen.insert(target))
        .then_some(())
        .ok_or("bootstrap targets must be unique")
}

fn repository_name(arguments: &Arguments) -> Result<&str, &'static str> {
    arguments
        .repository
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or("repository name is required")
}

fn create_operation_id(owner_kind: &OwnerKind) -> &'static str {
    match owner_kind {
        OwnerKind::CurrentUser => "createCurrentUserRepo",
        OwnerKind::Organization => "createOrgRepo",
        OwnerKind::User => "adminCreateRepo",
    }
}

fn create_arguments(arguments: &Arguments) -> Map<String, Value> {
    match arguments.owner_kind {
        OwnerKind::CurrentUser => {
            Map::from_iter([("body".to_string(), json!(arguments.repository))])
        }
        OwnerKind::Organization => Map::from_iter([
            ("org".to_string(), json!(arguments.owner)),
            ("body".to_string(), json!(arguments.repository)),
        ]),
        OwnerKind::User => Map::from_iter([
            ("username".to_string(), json!(arguments.owner)),
            ("repository".to_string(), json!(arguments.repository)),
        ]),
    }
}

fn repository_arguments(owner: &str, repository: &str) -> Map<String, Value> {
    Map::from_iter([
        ("owner".to_string(), json!(owner)),
        ("repo".to_string(), json!(repository)),
    ])
}

fn repository_body_arguments(
    owner: &str,
    repository: &str,
    body: Map<String, Value>,
) -> Map<String, Value> {
    let mut arguments = repository_arguments(owner, repository);
    arguments.insert("body".to_string(), Value::Object(body));
    arguments
}

fn repository_named_body_arguments(
    owner: &str,
    repository: &str,
    name_key: &str,
    name: &str,
    body: Map<String, Value>,
) -> Map<String, Value> {
    let mut arguments = repository_body_arguments(owner, repository, body);
    arguments.insert(name_key.to_string(), json!(name));
    arguments
}

fn operation_property_schema(operation_id: &str, property: &str) -> Value {
    let operation = exposed_operation_by_id(operation_id)
        .expect("bootstrap schema operation must exist in the pinned catalog");
    let schema = operation
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|properties| properties.get(property))
        .expect("bootstrap schema property must exist");
    resolve_refs(schema, &Value::Object(operation.input_schema.clone()), 0)
}

fn resolve_refs(value: &Value, root: &Value, depth: usize) -> Value {
    assert!(depth < 64, "generated bootstrap schema reference cycle");
    match value {
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                let pointer = reference
                    .strip_prefix('#')
                    .expect("generated schema references must be local");
                return resolve_refs(
                    root.pointer(pointer)
                        .expect("generated schema reference must resolve"),
                    root,
                    depth + 1,
                );
            }
            Value::Object(
                object
                    .iter()
                    .map(|(key, value)| (key.clone(), resolve_refs(value, root, depth + 1)))
                    .collect(),
            )
        }
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| resolve_refs(value, root, depth + 1))
                .collect(),
        ),
        value => value.clone(),
    }
}

fn object_schema(properties: Value, required: &[&str]) -> Map<String, Value> {
    let mut schema = Map::from_iter([
        ("type".to_string(), Value::String("object".to_string())),
        ("properties".to_string(), properties),
        ("additionalProperties".to_string(), Value::Bool(false)),
    ]);
    if !required.is_empty() {
        schema.insert("required".to_string(), json!(required));
    }
    schema
}

fn output_schema() -> Map<String, Value> {
    crate::displaceable_output_schema(
        json!({
            "complete": {"type": "boolean"},
            "repository_created": {"type": "boolean"},
            "steps": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "operation_id": {"type": "string"},
                        "outcome": {
                            "type": "string",
                            "enum": ["applied", "unchanged", "failed"]
                        },
                        "status": {"type": ["integer", "null"]},
                        "detail": {"type": "string"}
                    },
                    "required": ["name", "operation_id", "outcome", "status", "detail"],
                    "additionalProperties": false
                }
            },
            "compensations": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "tool": {"type": "string"},
                        "operation_id": {"type": "string"},
                        "arguments": {"type": "object", "additionalProperties": true},
                        "reason": {"type": "string"}
                    },
                    "required": ["tool", "operation_id", "arguments", "reason"],
                    "additionalProperties": false
                }
            },
            "access_token": {
                "oneOf": [
                    {"type": "null"},
                    Value::Object(super::access_token_created_schema())
                ]
            }
        }),
        &[
            "complete",
            "repository_created",
            "steps",
            "compensations",
            "access_token",
        ],
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const REPO_GET: &str = "GET /api/v1/repos/acme/widget HTTP/1.1";
    const ORG_CREATE: &str = "POST /api/v1/orgs/acme/repos HTTP/1.1";
    const TOKEN_GET: &str = "GET /api/v1/users/token-user/tokens?page=1&limit=100 HTTP/1.1";
    const PROTECTION_GET: &str = "GET /api/v1/repos/acme/widget/branch_protections/main HTTP/1.1";
    const PROTECTION_CREATE: &str = "POST /api/v1/repos/acme/widget/branch_protections HTTP/1.1";

    #[test]
    fn tool_publishes_closed_typed_and_sensitive_contract() {
        let tool = tool();
        let schema = Value::Object(tool.input_schema.as_ref().clone());
        assert_eq!(schema["additionalProperties"], false);
        assert!(
            schema
                .pointer("/properties/repository/properties/name")
                .is_some()
        );
        let protection = schema
            .pointer("/properties/branch_protections/items")
            .expect("branch protection schema");
        assert_eq!(protection["required"], json!(["rule_name"]));
        assert!(protection.pointer("/properties/branch_name").is_none());
        let meta = tool.meta.expect("metadata");
        assert_eq!(meta.0["org.cacahuate/sensitiveResult"], true);
        assert_eq!(meta.0["org.cacahuate/sensitiveInput"], true);
        assert_eq!(
            schema["properties"]["actions_secrets"]["items"]["additionalProperties"],
            false
        );
        assert_eq!(
            schema["properties"]["hooks"]["items"]["additionalProperties"],
            false
        );
        assert_eq!(
            schema["properties"]["labels"]["items"]["properties"]["name"]["maxLength"],
            1_024
        );
        assert_eq!(
            schema["properties"]["hooks"]["items"]["properties"]["events"]["maxItems"],
            100
        );
        assert_eq!(
            schema["properties"]["hooks"]["items"]["properties"]["config"]["maxProperties"],
            32
        );
        assert_eq!(
            schema["properties"]["hooks"]["items"]["properties"]["config"]["required"],
            json!(["content_type", "url"])
        );
        assert_eq!(
            schema["properties"]["hooks"]["items"]["properties"]["config"]["additionalProperties"]
                ["maxLength"],
            16_384
        );
        assert_eq!(
            schema["properties"]["deploy_keys"]["items"]["properties"]["key"]["maxLength"],
            16_384
        );
    }

    #[test]
    fn declaration_validation_rejects_every_invalid_step_before_execution() {
        let arguments = parse(Some(
            json!({
                "owner": "acme",
                "owner_kind": "organization",
                "repository": {"name": "widget", "private": true},
                "collaborators": [{"username": "alice", "permission": "owner"}]
            })
            .as_object()
            .expect("arguments")
            .clone(),
        ))
        .expect("parse");
        assert_eq!(
            validate(&arguments),
            Err("bootstrap declaration does not match a Gitea operation")
        );

        let empty_branch = parsed(&json!({
            "owner": "acme",
            "owner_kind": "organization",
            "repository": {"name": "widget"},
            "branch_protections": [{"rule_name": ""}]
        }));
        assert_eq!(
            validate(&empty_branch),
            Err("bootstrap contains an invalid path segment")
        );

        let user_team = parsed(&json!({
            "owner": "alice",
            "owner_kind": "user",
            "repository": {"name": "widget"},
            "teams": ["developers"]
        }));
        assert_eq!(
            validate(&user_team),
            Err("teams require an organization repository")
        );

        assert!(validate_unique_targets(["main", "main"]).is_err());

        let duplicate_topics = json!({
            "owner": "acme",
            "owner_kind": "organization",
            "repository": {"name": "widget"},
            "topics": ["mcp", "mcp"]
        })
        .as_object()
        .expect("arguments")
        .clone();
        assert_eq!(
            validate_input(&duplicate_topics),
            Err("arguments do not match the repository bootstrap schema")
        );

        let rename = json!({
            "owner": "acme",
            "owner_kind": "organization",
            "repository": {"name": "widget"},
            "settings": {"name": "renamed"}
        })
        .as_object()
        .expect("arguments")
        .clone();
        assert_eq!(
            validate_input(&rename),
            Err("arguments do not match the repository bootstrap schema")
        );

        let incomplete_hook = json!({
            "owner": "acme",
            "owner_kind": "organization",
            "repository": {"name": "widget"},
            "hooks": [{
                "type": "gitea",
                "config": {"url": "https://hooks.example.test/gitea"}
            }]
        })
        .as_object()
        .expect("arguments")
        .clone();
        assert_eq!(
            validate_input(&incomplete_hook),
            Err("arguments do not match the repository bootstrap schema")
        );

        let body_field_dot_segments = parsed(&json!({
            "owner": "acme",
            "owner_kind": "organization",
            "repository": {"name": "widget"},
            "labels": [{"name": ".", "color": "#d73a4a"}],
            "deploy_keys": [{
                "title": "..",
                "key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITest"
            }]
        }));
        assert!(validate(&body_field_dot_segments).is_ok());
    }

    #[tokio::test]
    async fn adopt_converges_common_setup_and_team_add_race() {
        let result = run_bootstrap(
            vec![
                (REPO_GET, "200 OK", "{}"),
                (
                    r#"PUT /api/v1/repos/acme/widget/topics HTTP/1.1||"topics":[]"#,
                    "204 No Content",
                    "",
                ),
                (
                    "GET /api/v1/repos/acme/widget/teams/developers HTTP/1.1",
                    "404 Not Found",
                    "{}",
                ),
                (
                    "PUT /api/v1/repos/acme/widget/teams/developers HTTP/1.1",
                    "422 Unprocessable Entity",
                    "{}",
                ),
                (
                    "GET /api/v1/repos/acme/widget/teams/developers HTTP/1.1",
                    "200 OK",
                    "{}",
                ),
                (
                    r#"PUT /api/v1/repos/acme/widget/collaborators/alice HTTP/1.1||"permission":"write""#,
                    "204 No Content",
                    "",
                ),
                (
                    PROTECTION_GET,
                    "404 Not Found",
                    "{}",
                ),
                (
                    r#"POST /api/v1/repos/acme/widget/branch_protections HTTP/1.1||"enable_status_check":true"#,
                    "201 Created",
                    "{}",
                ),
                (
                    r#"PATCH /api/v1/repos/acme/widget HTTP/1.1||"archived":true,"has_actions":true"#,
                    "200 OK",
                    "{}",
                ),
            ],
            json!({
                "owner": "acme",
                "owner_kind": "organization",
                "repository": {"name": "widget", "private": true},
                "if_exists": "adopt",
                "settings": {"archived": true, "has_actions": true},
                "topics": [],
                "teams": ["developers"],
                "collaborators": [{"username": "alice", "permission": "write"}],
                "branch_protections": [{
                    "rule_name": "main",
                    "enable_status_check": true,
                    "status_check_contexts": ["test / test"]
                }]
            }),
        )
        .await;

        assert!(result.complete);
        assert!(!result.repository_created);
        assert_eq!(result.steps.len(), 6);
        assert!(result.compensations.is_empty());
    }

    #[tokio::test]
    async fn adopt_converges_repository_extensions_in_order() {
        let result = run_bootstrap(
            vec![
                (REPO_GET, "200 OK", "{}"),
                (
                    "GET /api/v1/repos/acme/widget/labels?page=1&limit=100 HTTP/1.1",
                    "200 OK",
                    "[]",
                ),
                (
                    r#"POST /api/v1/repos/acme/widget/labels HTTP/1.1||"name":"priority/high""#,
                    "201 Created",
                    r#"{"id":11}"#,
                ),
                (
                    "GET /api/v1/repos/acme/widget/hooks?page=1&limit=100 HTTP/1.1",
                    "200 OK",
                    "[]",
                ),
                (
                    r#"POST /api/v1/repos/acme/widget/hooks HTTP/1.1||"url":"https://hooks.example.test/gitea""#,
                    "201 Created",
                    r#"{"id":12}"#,
                ),
                (
                    "GET /api/v1/repos/acme/widget/keys?page=1&limit=100 HTTP/1.1",
                    "200 OK",
                    "[]",
                ),
                (
                    r#"POST /api/v1/repos/acme/widget/keys HTTP/1.1||"title":"deploy""#,
                    "201 Created",
                    r#"{"id":13}"#,
                ),
                (
                    "GET /api/v1/repos/acme/widget/actions/variables/CI_MODE HTTP/1.1",
                    "404 Not Found",
                    "{}",
                ),
                (
                    r#"POST /api/v1/repos/acme/widget/actions/variables/CI_MODE HTTP/1.1||"value":"strict""#,
                    "201 Created",
                    "{}",
                ),
                (
                    r#"PUT /api/v1/repos/acme/widget/actions/secrets/DEPLOY_TOKEN HTTP/1.1||"data":"test-secret""#,
                    "201 Created",
                    "{}",
                ),
            ],
            extension_declaration("test-secret", false),
        )
        .await;

        assert!(result.complete);
        assert_eq!(result.steps.len(), 6);
        assert_eq!(
            result
                .steps
                .iter()
                .map(|step| step.operation_id.as_str())
                .collect::<Vec<_>>(),
            [
                "repoGet",
                "issueCreateLabel",
                "repoCreateHook",
                "repoCreateKey",
                "createRepoVariable",
                "updateRepoSecret"
            ]
        );
        assert!(result.compensations.is_empty());
        let encoded = serde_json::to_string(&result).expect("result");
        assert!(!encoded.contains("test-secret"));
    }

    #[tokio::test]
    async fn adopt_updates_existing_extensions_without_creation_compensations() {
        let result = run_bootstrap(
            vec![
                (REPO_GET, "200 OK", "{}"),
                (
                    "GET /api/v1/repos/acme/widget/labels?page=1&limit=100 HTTP/1.1",
                    "200 OK",
                    r#"[{"id":11,"name":"priority/high"}]"#,
                ),
                (
                    "GET /api/v1/repos/acme/widget/labels?page=2&limit=100 HTTP/1.1",
                    "200 OK",
                    "[]",
                ),
                (
                    r#"PATCH /api/v1/repos/acme/widget/labels/11 HTTP/1.1||"description":"urgent""#,
                    "200 OK",
                    "{}",
                ),
                (
                    "GET /api/v1/repos/acme/widget/hooks?page=1&limit=100 HTTP/1.1",
                    "200 OK",
                    r#"[{"id":12,"type":"gitea","config":{"content_type":"json","url":"https://hooks.example.test/gitea"}}]"#,
                ),
                (
                    "GET /api/v1/repos/acme/widget/hooks?page=2&limit=100 HTTP/1.1",
                    "200 OK",
                    "[]",
                ),
                (
                    r#"PATCH /api/v1/repos/acme/widget/hooks/12 HTTP/1.1||"active":true"#,
                    "200 OK",
                    "{}",
                ),
                (
                    "GET /api/v1/repos/acme/widget/keys?page=1&limit=100 HTTP/1.1",
                    "200 OK",
                    r#"[{"id":13,"title":"deploy","key":"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITest","read_only":true}]"#,
                ),
                (
                    "GET /api/v1/repos/acme/widget/keys?page=2&limit=100 HTTP/1.1",
                    "200 OK",
                    "[]",
                ),
                (
                    "GET /api/v1/repos/acme/widget/actions/variables/CI_MODE HTTP/1.1",
                    "200 OK",
                    r#"{"name":"CI_MODE","value":"old"}"#,
                ),
                (
                    r#"PUT /api/v1/repos/acme/widget/actions/variables/CI_MODE HTTP/1.1||"value":"strict""#,
                    "204 No Content",
                    "",
                ),
                (
                    r#"PUT /api/v1/repos/acme/widget/actions/secrets/DEPLOY_TOKEN HTTP/1.1||"data":"replacement-secret""#,
                    "204 No Content",
                    "",
                ),
            ],
            extension_declaration("replacement-secret", true),
        )
        .await;

        assert!(result.complete);
        assert_eq!(
            result
                .steps
                .iter()
                .map(|step| step.operation_id.as_str())
                .collect::<Vec<_>>(),
            [
                "repoGet",
                "issueEditLabel",
                "repoEditHook",
                "repoListKeys",
                "updateRepoVariable",
                "updateRepoSecret"
            ]
        );
        assert!(result.compensations.is_empty());
        assert!(
            !serde_json::to_string(&result)
                .expect("result")
                .contains("replacement-secret")
        );
    }

    #[tokio::test]
    async fn extension_failure_returns_only_created_resource_compensations() {
        let result = run_bootstrap(
            vec![
                (REPO_GET, "200 OK", "{}"),
                (
                    "GET /api/v1/repos/acme/widget/labels?page=1&limit=100 HTTP/1.1",
                    "200 OK",
                    "[]",
                ),
                (
                    r#"POST /api/v1/repos/acme/widget/labels HTTP/1.1||"name":"priority/high""#,
                    "201 Created",
                    r#"{"id":11}"#,
                ),
                (
                    "GET /api/v1/repos/acme/widget/hooks?page=1&limit=100 HTTP/1.1",
                    "200 OK",
                    "[]",
                ),
                (
                    r#"POST /api/v1/repos/acme/widget/hooks HTTP/1.1||"url":"https://hooks.example.test/gitea""#,
                    "422 Unprocessable Entity",
                    "{}",
                ),
            ],
            json!({
                "owner": "acme",
                "owner_kind": "organization",
                "repository": {"name": "widget"},
                "if_exists": "adopt",
                "labels": [{
                    "name": "priority/high",
                    "color": "#d73a4a"
                }],
                "hooks": [{
                    "type": "gitea",
                    "config": {
                        "content_type": "json",
                        "url": "https://hooks.example.test/gitea"
                    }
                }]
            }),
        )
        .await;

        assert!(!result.complete);
        assert_eq!(result.compensations.len(), 1);
        assert_eq!(result.compensations[0].tool, "issue.delete_label");
        assert_eq!(result.compensations[0].operation_id, "issueDeleteLabel");
        assert_eq!(result.compensations[0].arguments["id"], 11);
        assert_eq!(result.steps.last().expect("failure").status, Some(422));
    }

    #[tokio::test]
    async fn updated_secret_never_receives_creation_compensation() {
        let result = run_bootstrap(
            vec![
                (REPO_GET, "200 OK", "{}"),
                (
                    r#"PUT /api/v1/repos/acme/widget/actions/secrets/DEPLOY_TOKEN HTTP/1.1||"data":"replacement-secret""#,
                    "204 No Content",
                    "",
                ),
                (PROTECTION_GET, "500 Internal Server Error", "{}"),
            ],
            json!({
                "owner": "acme",
                "owner_kind": "organization",
                "repository": {"name": "widget"},
                "if_exists": "adopt",
                "actions_secrets": [{
                    "name": "DEPLOY_TOKEN",
                    "data": "replacement-secret"
                }],
                "branch_protections": [{"rule_name": "main"}]
            }),
        )
        .await;

        assert!(!result.complete);
        assert!(
            result
                .compensations
                .iter()
                .all(|item| item.operation_id != "deleteRepoSecret")
        );
        assert_eq!(result.steps.last().expect("failure").status, Some(500));
    }

    #[tokio::test]
    async fn successful_create_without_id_is_reported_as_uncompensatable() {
        let result = run_bootstrap(
            vec![
                (REPO_GET, "200 OK", "{}"),
                (
                    "GET /api/v1/repos/acme/widget/labels?page=1&limit=100 HTTP/1.1",
                    "200 OK",
                    "[]",
                ),
                (
                    r#"POST /api/v1/repos/acme/widget/labels HTTP/1.1||"name":"priority/high""#,
                    "201 Created",
                    "{}",
                ),
            ],
            json!({
                "owner": "acme",
                "owner_kind": "organization",
                "repository": {"name": "widget"},
                "if_exists": "adopt",
                "labels": [{
                    "name": "priority/high",
                    "color": "#d73a4a"
                }]
            }),
        )
        .await;

        assert!(!result.complete);
        assert_eq!(result.steps[result.steps.len() - 2].outcome, "applied");
        assert_eq!(result.steps.last().expect("failure").outcome, "failed");
        assert!(result.compensations.is_empty());
    }

    #[tokio::test]
    async fn a_token_search_bound_hit_records_the_call_that_reached_gitea() {
        // The list answered 200; what was exceeded is this workflow's own search
        // bound. Recording no status would say the request never ran, which is
        // false about a call Gitea served.
        let tokens = (0..100)
            .map(|id| json!({"id": id + 1, "name": format!("other-{id}"), "scopes": []}))
            .collect::<Vec<_>>();
        let mut responses = vec![(REPO_GET, "200 OK", "{}")];
        for page in 1..=MAX_TOKEN_SEARCH_PAGES {
            let request: &'static str = Box::leak(
                format!("GET /api/v1/users/token-user/tokens?page={page}&limit=100 HTTP/1.1")
                    .into_boxed_str(),
            );
            responses.push((request, "200 OK", leak_json(&tokens)));
        }
        let result = run_bootstrap(
            responses,
            json!({
                "owner": "acme",
                "owner_kind": "organization",
                "repository": {"name": "widget"},
                "if_exists": "adopt",
                "access_token": {"name": "agent", "scopes": ["read:user"]}
            }),
        )
        .await;

        assert!(!result.complete);
        let failed = result.steps.last().expect("failed step");
        assert_eq!(failed.operation_id, "userGetTokens");
        assert_eq!(
            failed.status,
            Some(200),
            "the search bound was hit on a response Gitea delivered: {}",
            failed.detail
        );
        assert!(
            !failed.detail.contains("never sent"),
            "the request plainly was sent: {}",
            failed.detail
        );
    }

    #[tokio::test]
    async fn token_lookup_finds_a_later_page_under_a_server_page_cap_without_creating() {
        let tokens = (1..=50)
            .map(|id| json!({"id":id,"name":format!("other-{id}"),"scopes":[]}))
            .collect::<Vec<_>>();
        let result = run_bootstrap(vec![
            (REPO_GET, "200 OK", "{}"),
            (TOKEN_GET, "200 OK", leak_json(&tokens)),
            ("GET /api/v1/users/token-user/tokens?page=2&limit=100 HTTP/1.1", "200 OK", r#"[{"id":51,"name":"agent","scopes":["read:user"]}]"#),
        ], json!({"owner":"acme","owner_kind":"organization","repository":{"name":"widget"},"if_exists":"adopt","access_token":{"name":"agent","scopes":["read:user"]}})).await;
        assert!(result.complete);
        assert!(
            result.access_token.is_none(),
            "an existing token is adopted without minting a new value"
        );
    }

    #[tokio::test]
    async fn token_creation_waits_for_empty_page_and_page_failure_never_creates() {
        for failed in [false, true] {
            let mut responses = vec![
                (REPO_GET, "200 OK", "{}"),
                (
                    TOKEN_GET,
                    "200 OK",
                    r#"[{"id":1,"name":"other","scopes":[]}]"#,
                ),
                (
                    "GET /api/v1/users/token-user/tokens?page=2&limit=100 HTTP/1.1",
                    if failed {
                        "503 Service Unavailable"
                    } else {
                        "200 OK"
                    },
                    if failed { "{}" } else { "[]" },
                ),
            ];
            if !failed {
                responses.push(("POST /api/v1/users/token-user/tokens HTTP/1.1", "201 Created", r#"{"id":2,"name":"agent","sha1":"synthetic-created-token","scopes":["read:user"]}"#));
            }
            let result = run_bootstrap(responses, json!({"owner":"acme","owner_kind":"organization","repository":{"name":"widget"},"if_exists":"adopt","access_token":{"name":"agent","scopes":["read:user"]}})).await;
            assert_eq!(result.complete, !failed);
            assert_eq!(result.access_token.is_some(), !failed);
            if failed {
                assert_eq!(result.steps.last().unwrap().status, Some(503));
            }
        }
    }

    #[tokio::test]
    async fn full_inventory_page_is_convergent_when_the_next_page_is_empty() {
        let mut labels = (0..99)
            .map(|id| json!({"id": id + 2, "name": format!("label-{id}")}))
            .collect::<Vec<_>>();
        labels.push(json!({"id": 1, "name": "priority/high"}));
        let result = run_bootstrap(
            vec![
                (REPO_GET, "200 OK", "{}"),
                (
                    "GET /api/v1/repos/acme/widget/labels?page=1&limit=100 HTTP/1.1",
                    "200 OK",
                    leak_json(&labels),
                ),
                (
                    "GET /api/v1/repos/acme/widget/labels?page=2&limit=100 HTTP/1.1",
                    "200 OK",
                    "[]",
                ),
                (
                    r#"PATCH /api/v1/repos/acme/widget/labels/1 HTTP/1.1||"description":"urgent""#,
                    "200 OK",
                    "{}",
                ),
            ],
            json!({
                "owner": "acme",
                "owner_kind": "organization",
                "repository": {"name": "widget"},
                "if_exists": "adopt",
                "labels": [{
                    "name": "priority/high",
                    "color": "#d73a4a",
                    "description": "urgent"
                }]
            }),
        )
        .await;

        assert!(result.complete);
        assert_eq!(
            result.steps.last().expect("label").operation_id,
            "issueEditLabel"
        );
    }

    #[tokio::test]
    async fn partial_failure_returns_repository_compensation_without_rollback() {
        let result = run_bootstrap(
            vec![
                (REPO_GET, "404 Not Found", "{}"),
                (ORG_CREATE, "201 Created", "{}"),
                (PROTECTION_GET, "404 Not Found", "{}"),
                (PROTECTION_CREATE, "201 Created", "{}"),
                (TOKEN_GET, "200 OK", "[]"),
                (
                    "POST /api/v1/users/token-user/tokens HTTP/1.1",
                    "422 Unprocessable Entity",
                    "{}",
                ),
            ],
            json!({
                "owner": "acme",
                "owner_kind": "organization",
                "repository": {"name": "widget", "private": true},
                "branch_protections": [{"rule_name": "main"}],
                "access_token": {
                    "name": "agent",
                    "scopes": ["write:repository"]
                }
            }),
        )
        .await;

        assert!(!result.complete);
        assert!(result.repository_created);
        assert_eq!(result.compensations.len(), 2);
        assert_eq!(result.compensations[0].tool, "repository.delete");
        assert_eq!(result.compensations[0].operation_id, "repoDelete");
        assert_eq!(
            result.compensations[0].arguments.get("repo"),
            Some(&json!("widget"))
        );
        assert_eq!(
            result.compensations[1].tool,
            "repository.delete_branch_protection"
        );
        assert_eq!(
            result.compensations[1].operation_id,
            "repoDeleteBranchProtection"
        );
        assert_eq!(result.steps.last().expect("failed step").status, Some(422));
    }

    #[tokio::test]
    async fn a_failed_step_records_what_gitea_objected_to() {
        // A bootstrap that reports only "Gitea rejected the declared state"
        // tells its caller to abandon the workflow without telling them what to
        // change. The generated lane already normalizes and redacts the reason;
        // the step detail carries it through.
        let result = run_bootstrap(
            vec![
                (REPO_GET, "404 Not Found", "{}"),
                (
                    ORG_CREATE,
                    "422 Unprocessable Entity",
                    r#"{"message":"repository name is already in use"}"#,
                ),
            ],
            json!({
                "owner": "acme",
                "owner_kind": "organization",
                "repository": {"name": "widget"}
            }),
        )
        .await;

        assert!(!result.complete);
        let failed = result.steps.last().expect("failed step");
        assert_eq!(failed.status, Some(422));
        assert!(
            failed.detail.contains("repository name is already in use"),
            "the reason survives rather than a fixed sentence: {}",
            failed.detail
        );
        assert!(
            failed.detail.contains("Gitea rejected repository creation"),
            "and which step produced it is still named: {}",
            failed.detail
        );
    }

    #[tokio::test]
    async fn an_extension_refusal_also_records_what_gitea_objected_to() {
        // The extension steps are a separate module with their own refusal
        // sites, and fixing the ones in this file left those reporting fixed
        // text. A caller cannot tell which module handled a step, so the
        // guarantee has to hold across both.
        let result = run_bootstrap(
            vec![
                (REPO_GET, "200 OK", "{}"),
                (
                    "GET /api/v1/repos/acme/widget/labels?page=1&limit=100 HTTP/1.1",
                    "200 OK",
                    "[]",
                ),
                (
                    r#"POST /api/v1/repos/acme/widget/labels HTTP/1.1||"name":"priority/high""#,
                    "201 Created",
                    r#"{"id":11}"#,
                ),
                (
                    "GET /api/v1/repos/acme/widget/actions/variables/CI_MODE HTTP/1.1",
                    "403 Forbidden",
                    r#"{"message":"actions are disabled for this repository"}"#,
                ),
            ],
            json!({
                "owner": "acme",
                "owner_kind": "organization",
                "repository": {"name": "widget"},
                "if_exists": "adopt",
                "labels": [{"name": "priority/high", "color": "#d73a4a"}],
                "actions_variables": [{"name": "CI_MODE", "value": "strict"}]
            }),
        )
        .await;

        assert!(!result.complete);
        let failed = result.steps.last().expect("failed step");
        assert_eq!(failed.operation_id, "getRepoVariable");
        assert_eq!(failed.status, Some(403));
        assert!(
            failed
                .detail
                .contains("actions are disabled for this repository"),
            "the reason crosses the module boundary: {}",
            failed.detail
        );
    }

    #[tokio::test]
    async fn a_step_with_an_unreadable_refusal_still_says_it_failed() {
        // The fallback must be a generic reason, never an empty one: a blank
        // detail reads as though nothing went wrong.
        let result = run_bootstrap(
            vec![
                (REPO_GET, "404 Not Found", "{}"),
                (ORG_CREATE, "500 Internal Server Error", "{}"),
            ],
            json!({
                "owner": "acme",
                "owner_kind": "organization",
                "repository": {"name": "widget"}
            }),
        )
        .await;

        assert!(!result.complete);
        let failed = result.steps.last().expect("failed step");
        assert_eq!(failed.status, Some(500));
        assert!(
            !failed.detail.trim().is_empty(),
            "a step that failed must say so"
        );
    }

    #[tokio::test]
    async fn repository_race_reports_reread() {
        let result = run_bootstrap(
            vec![
                (REPO_GET, "404 Not Found", "{}"),
                (ORG_CREATE, "409 Conflict", "{}"),
                (
                    REPO_GET,
                    "404 Not Found",
                    r#"{"message":"the repository does not exist"}"#,
                ),
            ],
            json!({
                "owner": "acme",
                "owner_kind": "organization",
                "repository": {"name": "widget"},
                "if_exists": "adopt"
            }),
        )
        .await;

        assert!(!result.complete);
        let failure = result.steps.last().expect("failed step");
        assert_eq!(failure.operation_id, "repoGet");
        // The reconciliation reread is its own refusal path, and it discarded
        // the reason after the direct paths were fixed.
        assert!(
            failure.detail.contains("the repository does not exist"),
            "the reread carries what Gitea said: {}",
            failure.detail
        );
        assert!(
            failure
                .detail
                .contains("conflicted repository is unreadable"),
            "and still says which reconciliation failed: {}",
            failure.detail
        );
    }

    #[tokio::test]
    async fn branch_protection_422_race() {
        let declaration = json!({
            "owner": "acme",
            "owner_kind": "organization",
            "repository": {"name": "widget"},
            "if_exists": "adopt",
            "branch_protections": [{"rule_name": "main"}]
        });
        let result = run_bootstrap(
            vec![
                (REPO_GET, "200 OK", "{}"),
                (PROTECTION_GET, "404 Not Found", "{}"),
                (PROTECTION_CREATE, "422 Unprocessable Entity", "{}"),
                (PROTECTION_GET, "200 OK", "{}"),
                (
                    "PATCH /api/v1/repos/acme/widget/branch_protections/main HTTP/1.1",
                    "200 OK",
                    "{}",
                ),
            ],
            declaration.clone(),
        )
        .await;

        assert!(result.complete);

        let rejected = run_bootstrap(
            vec![
                (REPO_GET, "200 OK", "{}"),
                (PROTECTION_GET, "404 Not Found", "{}"),
                (PROTECTION_CREATE, "422 Unprocessable Entity", "{}"),
                (PROTECTION_GET, "404 Not Found", "{}"),
            ],
            declaration,
        )
        .await;
        assert!(!rejected.complete);
        let failed = rejected.steps.last().expect("failed step");
        assert_eq!(failed.operation_id, "repoGetBranchProtection");
    }

    #[tokio::test]
    async fn access_token_scope_mismatch_fails_without_creation() {
        let (upstream_url, upstream) = loopback_responses(vec![(
            TOKEN_GET,
            "200 OK",
            r#"[{"id":1,"name":"agent","scopes":["read:repository"],"token_last_eight":"abcdefgh","created_at":null,"last_used_at":null}]"#,
        )])
        .await;
        let arguments = parsed(&json!({
            "owner": "acme",
            "owner_kind": "organization",
            "repository": {"name": "widget"},
            "access_token": {
                "name": "agent",
                "scopes": ["write:repository"]
            }
        }));
        let mut result = BootstrapResult::new();
        assert!(!apply_access_token(&token_client(&upstream_url), &arguments, &mut result).await);
        assert_eq!(result.steps[0].outcome, "failed");
        upstream.await.expect("upstream");
    }

    fn extension_declaration(secret: &str, active_hook: bool) -> Value {
        json!({
            "owner": "acme",
            "owner_kind": "organization",
            "repository": {"name": "widget"},
            "if_exists": "adopt",
            "labels": [{
                "name": "priority/high",
                "color": "#d73a4a",
                "description": "urgent"
            }],
            "hooks": [{
                "type": "gitea",
                "active": active_hook,
                "config": {
                    "content_type": "json",
                    "url": "https://hooks.example.test/gitea"
                },
                "events": ["push"]
            }],
            "deploy_keys": [{
                "title": "deploy",
                "key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITest",
                "read_only": true
            }],
            "actions_variables": [{
                "name": "CI_MODE",
                "value": "strict"
            }],
            "actions_secrets": [{
                "name": "DEPLOY_TOKEN",
                "data": secret
            }]
        })
    }

    fn parsed(value: &Value) -> Arguments {
        parse(Some(value.as_object().expect("arguments").clone())).expect("parse")
    }

    fn leak_json(value: &impl Serialize) -> &'static str {
        Box::leak(
            serde_json::to_string(value)
                .expect("test JSON")
                .into_boxed_str(),
        )
    }

    async fn run_bootstrap(
        responses: Vec<(&'static str, &'static str, &'static str)>,
        declaration: Value,
    ) -> BootstrapResult {
        let (upstream_url, upstream) = loopback_responses(responses).await;
        let arguments = parsed(&declaration);
        validate(&arguments).expect("valid declaration");
        let tokens = arguments
            .requires_token_administration()
            .then(|| token_client(&upstream_url));
        let result = execute(&client(&upstream_url), tokens.as_ref(), arguments).await;
        upstream.await.expect("upstream");
        result
    }

    fn client(base_url: &str) -> GiteaClient {
        GiteaClient::new(base_url, "service-token", Duration::from_secs(2)).expect("client")
    }

    fn token_client(base_url: &str) -> TokenLifecycleClient {
        TokenLifecycleClient::new(
            base_url,
            "token-user",
            "token-password",
            Duration::from_secs(2),
        )
        .expect("token client")
    }

    async fn loopback_responses(
        responses: Vec<(&'static str, &'static str, &'static str)>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let upstream = tokio::spawn(async move {
            for (expected_line, status, body) in responses {
                let (mut socket, _) = listener.accept().await.expect("request");
                let mut request = vec![0_u8; 32 * 1024];
                let length = socket.read(&mut request).await.expect("read");
                let request = String::from_utf8_lossy(&request[..length]);
                let (expected_line, expected_body) = expected_line
                    .split_once("||")
                    .unwrap_or((expected_line, ""));
                assert!(request.starts_with(expected_line), "{request}");
                assert!(request.contains(expected_body), "{request}");
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("response");
            }
        });
        (format!("http://{address}"), upstream)
    }
}
