use std::collections::HashSet;

use gitea_api::{ApiError, GiteaClient, OperationResponse};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{
    BootstrapResult, Compensation, apply_operation, invoke, operation_property_schema, push_step,
    repository_arguments, repository_body_arguments, validate_operation, validate_path,
};

const MAX_ITEMS: usize = 100;
const MAX_MAP_PROPERTIES: usize = 32;
const LABEL_STRING_MAX: usize = 1_024;
const RESOURCE_STRING_MAX: usize = 16_384;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Arguments {
    #[serde(default)]
    labels: Vec<Map<String, Value>>,
    #[serde(default)]
    hooks: Vec<Map<String, Value>>,
    #[serde(default)]
    deploy_keys: Vec<Map<String, Value>>,
    #[serde(default)]
    actions_variables: Vec<Variable>,
    #[serde(default)]
    actions_secrets: Vec<Secret>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Variable {
    name: String,
    value: String,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Secret {
    name: String,
    data: String,
    description: Option<String>,
}

pub(super) fn schema_properties() -> Map<String, Value> {
    Map::from_iter([
        ("labels".to_string(), array_schema(label_schema())),
        ("hooks".to_string(), array_schema(hook_schema())),
        (
            "deploy_keys".to_string(),
            array_schema(bounded_operation_schema(
                "repoCreateKey",
                RESOURCE_STRING_MAX,
            )),
        ),
        (
            "actions_variables".to_string(),
            array_schema(json!({
                "type": "object",
                "properties": {
                    "name": target_name_schema(),
                    "value": {"type": "string", "maxLength": 65536},
                    "description": {"type": "string", "maxLength": 1024}
                },
                "required": ["name", "value"],
                "additionalProperties": false
            })),
        ),
        (
            "actions_secrets".to_string(),
            array_schema(json!({
                "type": "object",
                "properties": {
                    "name": target_name_schema(),
                    "data": {"type": "string", "minLength": 1, "maxLength": 65536},
                    "description": {"type": "string", "maxLength": 1024}
                },
                "required": ["name", "data"],
                "additionalProperties": false
            })),
        ),
    ])
}

fn array_schema(items: Value) -> Value {
    Value::Object(Map::from_iter([
        ("type".to_string(), json!("array")),
        ("maxItems".to_string(), json!(MAX_ITEMS)),
        ("items".to_string(), items),
    ]))
}

fn target_name_schema() -> Value {
    json!({"type": "string", "minLength": 1, "maxLength": 255})
}

fn label_schema() -> Value {
    bounded_operation_schema("issueCreateLabel", LABEL_STRING_MAX)
}

fn hook_schema() -> Value {
    let mut schema = bounded_operation_schema("repoCreateHook", RESOURCE_STRING_MAX);
    let config = schema
        .pointer_mut("/properties/config")
        .and_then(Value::as_object_mut)
        .expect("resolved hook config schema");
    config.insert(
        "properties".to_string(),
        json!({
            "content_type": {
                "type": "string",
                "minLength": 1,
                "maxLength": RESOURCE_STRING_MAX
            },
            "url": {
                "type": "string",
                "minLength": 1,
                "maxLength": RESOURCE_STRING_MAX
            }
        }),
    );
    config.insert("required".to_string(), json!(["content_type", "url"]));
    schema
}

fn bounded_operation_schema(operation_id: &str, max_string_length: usize) -> Value {
    let mut schema = operation_property_schema(operation_id, "body");
    apply_schema_bounds(&mut schema, max_string_length);
    schema
}

fn apply_schema_bounds(schema: &mut Value, max_string_length: usize) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    match object.get("type").and_then(Value::as_str) {
        Some("string") => apply_numeric_limit(object, "maxLength", max_string_length),
        Some("array") => {
            apply_numeric_limit(object, "maxItems", MAX_ITEMS);
            if let Some(items) = object.get_mut("items") {
                apply_schema_bounds(items, max_string_length);
            }
        }
        Some("object") => {
            apply_numeric_limit(object, "maxProperties", MAX_MAP_PROPERTIES);
            if let Some(properties) = object.get_mut("properties").and_then(Value::as_object_mut) {
                for property in properties.values_mut() {
                    apply_schema_bounds(property, max_string_length);
                }
            }
            if let Some(additional) = object.get_mut("additionalProperties")
                && additional.is_object()
            {
                apply_schema_bounds(additional, max_string_length);
            }
        }
        _ => {}
    }
    if let Some(definitions) = object.get_mut("definitions").and_then(Value::as_object_mut) {
        for definition in definitions.values_mut() {
            apply_schema_bounds(definition, max_string_length);
        }
    }
    for keyword in ["allOf", "anyOf", "oneOf"] {
        if let Some(alternatives) = object.get_mut(keyword).and_then(Value::as_array_mut) {
            for alternative in alternatives {
                apply_schema_bounds(alternative, max_string_length);
            }
        }
    }
}

fn apply_numeric_limit(object: &mut Map<String, Value>, keyword: &str, limit: usize) {
    let effective = object
        .get(keyword)
        .and_then(Value::as_u64)
        .and_then(|current| usize::try_from(current).ok())
        .map_or(limit, |current| current.min(limit));
    object.insert(keyword.to_string(), json!(effective));
}

impl Arguments {
    pub(super) fn max_collection_len(&self) -> usize {
        [
            self.labels.len(),
            self.hooks.len(),
            self.deploy_keys.len(),
            self.actions_variables.len(),
            self.actions_secrets.len(),
        ]
        .into_iter()
        .max()
        .unwrap_or(0)
    }
}

pub(super) fn validate(
    arguments: &Arguments,
    owner: &str,
    repository: &str,
) -> Result<(), &'static str> {
    validate_inventory_operation("issueListLabels", owner, repository)?;
    validate_inventory_operation("repoListHooks", owner, repository)?;
    validate_inventory_operation("repoListKeys", owner, repository)?;

    validate_unique(
        arguments
            .labels
            .iter()
            .filter_map(|body| text(body, "name")),
    )?;
    for body in &arguments.labels {
        required_text(body, "name")?;
        validate_operation(
            "issueCreateLabel",
            repository_body_arguments(owner, repository, body.clone()),
        )?;
        validate_operation(
            "issueEditLabel",
            repository_id_body_arguments(owner, repository, 1, body.clone()),
        )?;
    }

    validate_unique(
        arguments
            .hooks
            .iter()
            .map(hook_identity)
            .collect::<Result<Vec<_>, _>>()?,
    )?;
    for body in &arguments.hooks {
        validate_operation(
            "repoCreateHook",
            repository_body_arguments(owner, repository, body.clone()),
        )?;
        validate_operation(
            "repoEditHook",
            repository_id_body_arguments(owner, repository, 1, hook_edit_body(body)),
        )?;
    }

    validate_unique(
        arguments
            .deploy_keys
            .iter()
            .filter_map(|body| text(body, "title")),
    )?;
    for body in &arguments.deploy_keys {
        required_text(body, "title")?;
        validate_operation(
            "repoCreateKey",
            repository_body_arguments(owner, repository, body.clone()),
        )?;
    }

    validate_unique(
        arguments
            .actions_variables
            .iter()
            .map(|item| item.name.as_str()),
    )?;
    for variable in &arguments.actions_variables {
        validate_path("variable", &variable.name)?;
        let path = named_arguments(owner, repository, "variablename", &variable.name);
        validate_operation("getRepoVariable", path.clone())?;
        validate_operation(
            "createRepoVariable",
            with_body(path.clone(), variable.create_body()),
        )?;
        validate_operation(
            "updateRepoVariable",
            with_body(path.clone(), variable.update_body()),
        )?;
        validate_operation("deleteRepoVariable", path)?;
    }

    validate_unique(
        arguments
            .actions_secrets
            .iter()
            .map(|item| item.name.as_str()),
    )?;
    for secret in &arguments.actions_secrets {
        validate_path("secret", &secret.name)?;
        let path = named_arguments(owner, repository, "secretname", &secret.name);
        validate_operation("updateRepoSecret", with_body(path.clone(), secret.body()))?;
        validate_operation("deleteRepoSecret", path)?;
    }
    Ok(())
}

fn validate_inventory_operation(
    operation_id: &str,
    owner: &str,
    repository: &str,
) -> Result<(), &'static str> {
    validate_operation(operation_id, inventory_arguments(owner, repository))
}

pub(super) async fn apply(
    client: &GiteaClient,
    arguments: &Arguments,
    owner: &str,
    repository: &str,
    result: &mut BootstrapResult,
) -> bool {
    apply_labels(client, &arguments.labels, owner, repository, result).await
        && apply_hooks(client, &arguments.hooks, owner, repository, result).await
        && apply_deploy_keys(client, &arguments.deploy_keys, owner, repository, result).await
        && apply_variables(
            client,
            &arguments.actions_variables,
            owner,
            repository,
            result,
        )
        .await
        && apply_secrets(
            client,
            &arguments.actions_secrets,
            owner,
            repository,
            result,
        )
        .await
}

async fn apply_labels(
    client: &GiteaClient,
    declared: &[Map<String, Value>],
    owner: &str,
    repository: &str,
    result: &mut BootstrapResult,
) -> bool {
    if declared.is_empty() {
        return true;
    }
    let Some(existing) = load_inventory(
        client,
        result,
        "labels.inventory",
        "issueListLabels",
        owner,
        repository,
    )
    .await
    else {
        return false;
    };
    for body in declared {
        let name = required_text(body, "name").expect("validated label");
        if let Some(item) = find_by_text(&existing, "name", name) {
            let Some(id) = item.get("id").and_then(Value::as_i64) else {
                result.failed_step(
                    format!("label.{name}"),
                    "issueListLabels",
                    Some(200),
                    "existing label has no id",
                );
                return false;
            };
            if !apply_operation(
                client,
                result,
                format!("label.{name}"),
                "issueEditLabel",
                repository_id_body_arguments(owner, repository, id, body.clone()),
            )
            .await
            {
                return false;
            }
        } else if !create_resource(
            client,
            result,
            format!("label.{name}"),
            "issueCreateLabel",
            repository_body_arguments(owner, repository, body.clone()),
            "issue.delete_label",
            "issueDeleteLabel",
            repository_arguments(owner, repository),
            "id",
        )
        .await
        {
            return false;
        }
    }
    true
}

async fn apply_hooks(
    client: &GiteaClient,
    declared: &[Map<String, Value>],
    owner: &str,
    repository: &str,
    result: &mut BootstrapResult,
) -> bool {
    if declared.is_empty() {
        return true;
    }
    let Some(existing) = load_inventory(
        client,
        result,
        "hooks.inventory",
        "repoListHooks",
        owner,
        repository,
    )
    .await
    else {
        return false;
    };
    for body in declared {
        let identity = hook_identity(body).expect("validated hook");
        let found = existing
            .iter()
            .find(|item| hook_identity_value(item).as_deref() == Some(identity.as_str()));
        if let Some(item) = found {
            let Some(id) = item.get("id").and_then(Value::as_i64) else {
                result.failed_step(
                    format!("hook.{identity}"),
                    "repoListHooks",
                    Some(200),
                    "existing hook has no id",
                );
                return false;
            };
            if !apply_operation(
                client,
                result,
                format!("hook.{identity}"),
                "repoEditHook",
                repository_id_body_arguments(owner, repository, id, hook_edit_body(body)),
            )
            .await
            {
                return false;
            }
        } else if !create_resource(
            client,
            result,
            format!("hook.{identity}"),
            "repoCreateHook",
            repository_body_arguments(owner, repository, body.clone()),
            "repository.delete_hook",
            "repoDeleteHook",
            repository_arguments(owner, repository),
            "id",
        )
        .await
        {
            return false;
        }
    }
    true
}

async fn apply_deploy_keys(
    client: &GiteaClient,
    declared: &[Map<String, Value>],
    owner: &str,
    repository: &str,
    result: &mut BootstrapResult,
) -> bool {
    if declared.is_empty() {
        return true;
    }
    let Some(existing) = load_inventory(
        client,
        result,
        "deploy_keys.inventory",
        "repoListKeys",
        owner,
        repository,
    )
    .await
    else {
        return false;
    };
    for body in declared {
        let title = required_text(body, "title").expect("validated deploy key");
        if let Some(item) = find_by_text(&existing, "title", title) {
            if item.get("key") != body.get("key")
                || body
                    .get("read_only")
                    .is_some_and(|value| item.get("read_only") != Some(value))
            {
                result.failed_step(
                    format!("deploy_key.{title}"),
                    "repoListKeys",
                    Some(200),
                    "same-title deploy key differs; delete before retrying",
                );
                return false;
            }
            push_step(
                result,
                format!("deploy_key.{title}"),
                "repoListKeys",
                "unchanged",
                Some(200),
                "matching deploy key exists",
            );
        } else if !create_resource(
            client,
            result,
            format!("deploy_key.{title}"),
            "repoCreateKey",
            repository_body_arguments(owner, repository, body.clone()),
            "repository.delete_deploy_key",
            "repoDeleteKey",
            repository_arguments(owner, repository),
            "id",
        )
        .await
        {
            return false;
        }
    }
    true
}

async fn apply_variables(
    client: &GiteaClient,
    declared: &[Variable],
    owner: &str,
    repository: &str,
    result: &mut BootstrapResult,
) -> bool {
    for variable in declared {
        let path = named_arguments(owner, repository, "variablename", &variable.name);
        match invoke(client, "getRepoVariable", path.clone()).await {
            Ok(response) if response.success => {
                if !apply_operation(
                    client,
                    result,
                    format!("actions_variable.{}", variable.name),
                    "updateRepoVariable",
                    with_body(path, variable.update_body()),
                )
                .await
                {
                    return false;
                }
            }
            Ok(response) if response.status == 404 => {
                if !create_named_resource(
                    client,
                    result,
                    format!("actions_variable.{}", variable.name),
                    "createRepoVariable",
                    with_body(path.clone(), variable.create_body()),
                    "repository.delete_repo_variable",
                    "deleteRepoVariable",
                    path,
                )
                .await
                {
                    return false;
                }
            }
            Ok(response) => {
                result.failed_step(
                    format!("actions_variable.{}", variable.name),
                    "getRepoVariable",
                    Some(response.status),
                    super::refusal_detail(&response, "Gitea rejected variable lookup"),
                );
                return false;
            }
            Err(error) => {
                let (status, detail) = super::failure_detail(&error);
                result.failed_step(
                    format!("actions_variable.{}", variable.name),
                    "getRepoVariable",
                    status,
                    detail,
                );
                return false;
            }
        }
    }
    true
}

async fn apply_secrets(
    client: &GiteaClient,
    declared: &[Secret],
    owner: &str,
    repository: &str,
    result: &mut BootstrapResult,
) -> bool {
    if declared.is_empty() {
        return true;
    }
    for secret in declared {
        let path = named_arguments(owner, repository, "secretname", &secret.name);
        match invoke(
            client,
            "updateRepoSecret",
            with_body(path.clone(), secret.body()),
        )
        .await
        {
            Ok(response) if response.success => {
                push_step(
                    result,
                    format!("actions_secret.{}", secret.name),
                    "updateRepoSecret",
                    "applied",
                    Some(response.status),
                    "secret converged",
                );
                if response.status == 201 {
                    result.compensations.push(Compensation {
                        tool: "repository.delete_repo_secret".to_string(),
                        operation_id: "deleteRepoSecret".to_string(),
                        arguments: path,
                        reason: "delete secret created before failure".to_string(),
                    });
                }
            }
            Ok(response) => {
                result.failed_step(
                    format!("actions_secret.{}", secret.name),
                    "updateRepoSecret",
                    Some(response.status),
                    super::refusal_detail(&response, "Gitea rejected secret update"),
                );
                return false;
            }
            Err(error) => {
                let (status, detail) = super::failure_detail(&error);
                result.failed_step(
                    format!("actions_secret.{}", secret.name),
                    "updateRepoSecret",
                    status,
                    detail,
                );
                return false;
            }
        }
    }
    true
}

async fn load_inventory(
    client: &GiteaClient,
    result: &mut BootstrapResult,
    step: &str,
    operation_id: &'static str,
    owner: &str,
    repository: &str,
) -> Option<Vec<Value>> {
    let mut inventory = Vec::new();
    for page in 1..=MAX_ITEMS + 1 {
        match invoke(
            client,
            operation_id,
            inventory_arguments_page(owner, repository, page),
        )
        .await
        {
            Ok(response) if response.success => {
                let Some(items) = response.data.as_array() else {
                    result.failed_step(
                        step,
                        operation_id,
                        Some(response.status),
                        "inventory response is not an array",
                    );
                    return None;
                };
                if items.is_empty() {
                    return Some(inventory);
                }
                if inventory.len() + items.len() > MAX_ITEMS {
                    result.failed_step(
                        step,
                        operation_id,
                        Some(response.status),
                        "inventory exceeds bootstrap search bound",
                    );
                    return None;
                }
                inventory.extend(items.iter().cloned());
            }
            Ok(response) => {
                result.failed_step(
                    step,
                    operation_id,
                    Some(response.status),
                    super::refusal_detail(&response, "Gitea rejected inventory lookup"),
                );
                return None;
            }
            Err(error) => {
                let (status, detail) = super::failure_detail(&error);
                result.failed_step(step, operation_id, status, detail);
                return None;
            }
        }
    }
    result.failed_step(
        step,
        operation_id,
        None,
        "inventory pagination exceeds bootstrap search bound",
    );
    None
}

#[allow(clippy::too_many_arguments)]
async fn create_resource(
    client: &GiteaClient,
    result: &mut BootstrapResult,
    step: String,
    operation_id: &'static str,
    arguments: Map<String, Value>,
    delete_tool: &'static str,
    delete_operation_id: &'static str,
    mut delete_arguments: Map<String, Value>,
    id_argument: &'static str,
) -> bool {
    match invoke(client, operation_id, arguments).await {
        Ok(response) if response.success => {
            let Some(id) = response.data.get("id").and_then(Value::as_i64) else {
                push_step(
                    result,
                    step.clone(),
                    operation_id,
                    "applied",
                    Some(response.status),
                    "resource created; response omitted its identifier",
                );
                result.failed_step(
                    step,
                    operation_id,
                    Some(response.status),
                    "automatic compensation unavailable because the response omitted its id",
                );
                return false;
            };
            push_step(
                result,
                step,
                operation_id,
                "applied",
                Some(response.status),
                "resource created",
            );
            delete_arguments.insert(id_argument.to_string(), json!(id));
            result.compensations.push(Compensation {
                tool: delete_tool.to_string(),
                operation_id: delete_operation_id.to_string(),
                arguments: delete_arguments,
                reason: "delete resource created before failure".to_string(),
            });
            true
        }
        response => record_mutation_failure(result, step, operation_id, response),
    }
}

#[allow(clippy::too_many_arguments)]
async fn create_named_resource(
    client: &GiteaClient,
    result: &mut BootstrapResult,
    step: String,
    operation_id: &'static str,
    arguments: Map<String, Value>,
    delete_tool: &'static str,
    delete_operation_id: &'static str,
    delete_arguments: Map<String, Value>,
) -> bool {
    match invoke(client, operation_id, arguments).await {
        Ok(response) if response.success => {
            push_step(
                result,
                step,
                operation_id,
                "applied",
                Some(response.status),
                "resource created",
            );
            result.compensations.push(Compensation {
                tool: delete_tool.to_string(),
                operation_id: delete_operation_id.to_string(),
                arguments: delete_arguments,
                reason: "delete resource created before failure".to_string(),
            });
            true
        }
        response => record_mutation_failure(result, step, operation_id, response),
    }
}

fn record_mutation_failure(
    result: &mut BootstrapResult,
    step: String,
    operation_id: &'static str,
    response: Result<OperationResponse, ApiError>,
) -> bool {
    match response {
        Ok(response) => result.failed_step(
            step,
            operation_id,
            Some(response.status),
            super::refusal_detail(&response, "Gitea rejected resource creation"),
        ),
        Err(error) => {
            let (status, detail) = super::failure_detail(&error);
            result.failed_step(step, operation_id, status, detail);
        }
    }
    false
}

impl Variable {
    fn create_body(&self) -> Map<String, Value> {
        optional_description(
            Map::from_iter([("value".to_string(), json!(self.value))]),
            self.description.as_deref(),
        )
    }

    fn update_body(&self) -> Map<String, Value> {
        self.create_body()
    }
}

impl Secret {
    fn body(&self) -> Map<String, Value> {
        optional_description(
            Map::from_iter([("data".to_string(), json!(self.data))]),
            self.description.as_deref(),
        )
    }
}

fn optional_description(
    mut body: Map<String, Value>,
    description: Option<&str>,
) -> Map<String, Value> {
    if let Some(description) = description {
        body.insert("description".to_string(), json!(description));
    }
    body
}

fn inventory_arguments(owner: &str, repository: &str) -> Map<String, Value> {
    inventory_arguments_page(owner, repository, 1)
}

fn inventory_arguments_page(owner: &str, repository: &str, page: usize) -> Map<String, Value> {
    let mut arguments = repository_arguments(owner, repository);
    arguments.insert("page".to_string(), json!(page));
    arguments.insert("limit".to_string(), json!(MAX_ITEMS));
    arguments
}

fn named_arguments(owner: &str, repository: &str, name: &str, value: &str) -> Map<String, Value> {
    let mut arguments = repository_arguments(owner, repository);
    arguments.insert(name.to_string(), json!(value));
    arguments
}

fn repository_id_body_arguments(
    owner: &str,
    repository: &str,
    id: i64,
    body: Map<String, Value>,
) -> Map<String, Value> {
    let mut arguments = repository_arguments(owner, repository);
    arguments.insert("id".to_string(), json!(id));
    with_body(arguments, body)
}

fn hook_edit_body(body: &Map<String, Value>) -> Map<String, Value> {
    let mut body = body.clone();
    body.remove("type");
    body
}

fn with_body(mut arguments: Map<String, Value>, body: Map<String, Value>) -> Map<String, Value> {
    arguments.insert("body".to_string(), Value::Object(body));
    arguments
}

fn required_text<'a>(body: &'a Map<String, Value>, name: &str) -> Result<&'a str, &'static str> {
    text(body, name).ok_or("bootstrap extension target is required")
}

fn text<'a>(body: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    body.get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn hook_identity(body: &Map<String, Value>) -> Result<String, &'static str> {
    let kind = required_text(body, "type")?;
    let config = body
        .get("config")
        .and_then(Value::as_object)
        .ok_or("hook config is required")?;
    let url = required_text(config, "url").map_err(|_| "hook config url is required")?;
    required_text(config, "content_type").map_err(|_| "hook config content_type is required")?;
    Ok(format!("{kind}:{url}"))
}

fn hook_identity_value(body: &Value) -> Option<String> {
    hook_identity(body.as_object()?).ok()
}

fn find_by_text<'a>(items: &'a [Value], name: &str, value: &str) -> Option<&'a Value> {
    items
        .iter()
        .find(|item| item.get(name).and_then(Value::as_str) == Some(value))
}

fn validate_unique<T>(targets: impl IntoIterator<Item = T>) -> Result<(), &'static str>
where
    T: AsRef<str>,
{
    let mut seen = HashSet::new();
    targets
        .into_iter()
        .all(|target| seen.insert(target.as_ref().to_string()))
        .then_some(())
        .ok_or("bootstrap extension targets must be unique")
}
