use std::fmt::Write as _;
use std::{borrow::Cow, sync::Arc};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64_STANDARD};
use gitea_api::{
    AccessTokenSelector, ApiError, CallOutcome, CreateAccessToken, GiteaClient,
    TokenLifecycleClient,
    catalog::{OperationRisk, OperationSpec, exposed_operation, operation_catalog},
};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResult, ContentBlock, Implementation, ListToolsResult, Meta,
        PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerInfo, Tool,
        ToolAnnotations,
    },
    service::RequestContext,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

mod bootstrap;
mod discovery;
pub mod files;
mod lanes;
mod repository_secret;
pub mod resources;
mod schema_portability;

/// Marks a result whose body carries credential material, so a gateway can act
/// on it without parsing the payload.
pub const SENSITIVE_RESULT_META: &str = "org.cacahuate/sensitiveResult";

/// Metadata key carrying how far a failed call got.
pub const OUTCOME_META: &str = "org.cacahuate/callOutcome";

pub const SERVER_VERSION_TOOL: &str = "server.version";
pub const ACCESS_TOKEN_CREATE_TOOL: &str = "access_token.create";
pub const ACCESS_TOKEN_LIST_TOOL: &str = "access_token.list";
pub const ACCESS_TOKEN_REVOKE_TOOL: &str = "access_token.revoke";

/// Orientation sent once at initialisation.
///
/// Deliberately static and free of counts that change with the catalog: it is
/// part of every prompt prefix, so a value that moves with the pinned
/// specification would invalidate the cache on each regeneration for no benefit
/// to the reader. What belongs here is what a caller cannot discover from a
/// single tool definition.
pub const SERVER_INSTRUCTIONS: &str = "\
Typed Gitea administration and repository automation, served as a layered \
surface. Select an operation first, then execute it: `catalog.search` filters \
the operation index by keyword, domain, risk, and administrative flag, \
`catalog.describe` returns one operation's contract including its exact input \
schema, and the `api.read`, `api.mutate`, `api.destroy`, and `api.admin` \
lanes execute an operation by tool name or upstream operation id with its \
arguments validated against that same schema. Each operation belongs to \
exactly one lane — its risk class, with administrative operations always on \
`api.admin` — and a call on the wrong lane is refused naming the right one. \
There is no raw-request or shell tool, by design.\n\n\
Lane results carry the executed operation's identity, risk, and \
administrative flag in `org.cacahuate/` metadata, and results that mint or \
return credentials carry an `org.cacahuate/sensitiveResult` marker; treat \
that marker as authoritative for handling. Oversized successful payloads are \
not inlined: they become `gitea-response:` handles read through \
`resources/read`, and a failed generated call reports how far it got in its \
`outcome` data — only `not_sent` is safe to reissue unchanged. \
`resources/list` also carries `gitea-catalog:/index`, the line-per-operation \
index behind `catalog.search`.\n\n\
`repository.secret.set_from_file` is the governed credential-input path: pass \
its `data_file` as an MCP file upload, never as inline model-visible text. The \
uploaded value is bounded, held only in memory, consumed once, and never \
returned.\n\n\
Access-token lifecycle operations use a separate credential lane and are \
exposed as `access_token.create`, `access_token.list`, and \
`access_token.revoke`. Callers cannot choose or supply upstream credentials.";

/// The lane that executes a catalog operation, named by tool name or upstream
/// operation id.
///
/// `None` for anything that is not a catalog operation — a hand-written tool,
/// a lane, or an unknown name — all of which are called directly or not at
/// all. This is the mapping a caller holding a name from the retired
/// per-operation surface needs, and the server answers with the same lane when
/// such a name reaches `tools/call`.
#[must_use]
pub fn lane_for(operation: &str) -> Option<&'static str> {
    exposed_operation(operation)
        .or_else(|| gitea_api::catalog::exposed_operation_by_id(operation))
        .map(|operation| lanes::lane_of(operation).tool_name())
}

/// URI of the operation index resource.
pub const CATALOG_INDEX_URI: &str = "gitea-catalog:/index";

/// Largest tool result placed directly in a reply.
///
/// Sized for a context window rather than for process safety. The transport
/// ceiling in `gitea-api` is three orders of magnitude larger and exists to stop
/// the process consuming unbounded memory; between the two, a payload becomes
/// retrievable as a resource instead of being inlined whole or refused.
pub const DEFAULT_CONTEXT_CEILING_BYTES: usize = 64 * 1024;

/// Longest prefix of an oversized text payload returned with its resource link,
/// so a caller can judge whether it wants the rest without a second call.
const RESOURCE_PREVIEW_BYTES: usize = 2 * 1024;

/// Smallest ceiling that can actually be honoured.
///
/// A displaced reply has an irreducible floor: the envelope, the payload handle,
/// and the resource link, all repeated as an escaped text block. Below roughly a
/// kilobyte there is nothing left to shed and the reply would exceed its own
/// ceiling, so a smaller configured value is raised to this one rather than
/// quietly violated.
pub const MINIMUM_CONTEXT_CEILING_BYTES: usize = 4 * 1024;

/// How oversized payloads are bounded and retained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LargeContentPolicy {
    /// Raised to [`MINIMUM_CONTEXT_CEILING_BYTES`] when a smaller value is
    /// configured, since a smaller one cannot be honoured.
    pub context_ceiling_bytes: usize,
    pub limits: resources::ResourceLimits,
}

impl Default for LargeContentPolicy {
    fn default() -> Self {
        Self {
            context_ceiling_bytes: DEFAULT_CONTEXT_CEILING_BYTES,
            limits: resources::ResourceLimits {
                max_object_bytes: 16 * 1024 * 1024,
                max_total_bytes: 64 * 1024 * 1024,
                time_to_live: std::time::Duration::from_mins(15),
            },
        }
    }
}

#[derive(Clone)]
pub struct GiteaMcp {
    client: Arc<GiteaClient>,
    token_client: Option<Arc<TokenLifecycleClient>>,
    policy: LargeContentPolicy,
    /// Shared by every session so that total retained bytes are bounded by the
    /// configured aggregate rather than by that aggregate times the number of
    /// live sessions.
    budget: Arc<resources::ResourceBudget>,
    store: Arc<resources::ResourceStore>,
    files: Option<Arc<files::FilePlane>>,
    execution_slots: Arc<tokio::sync::Semaphore>,
}

impl GiteaMcp {
    #[must_use]
    pub fn new(
        client: Arc<GiteaClient>,
        token_client: impl Into<Option<Arc<TokenLifecycleClient>>>,
    ) -> Self {
        Self::with_large_content_policy(client, token_client, LargeContentPolicy::default())
    }

    /// A handler for one client session, sharing upstream clients but owning its
    /// own resource store.
    ///
    /// Cloning a handler deliberately shares the store, because the transport
    /// clones it per request within a session. A session boundary is a different
    /// thing: displaced payloads are the caller's own reads, and nothing about
    /// being authenticated to this server entitles one conversation to another's
    /// job logs. The transport calls this once per session.
    #[must_use]
    pub fn new_session(&self) -> Self {
        Self {
            client: Arc::clone(&self.client),
            token_client: self.token_client.clone(),
            policy: self.policy,
            budget: Arc::clone(&self.budget),
            store: resources::ResourceStore::with_budget(
                self.policy.limits,
                Arc::clone(&self.budget),
            ),
            files: self.files.clone(),
            execution_slots: Arc::clone(&self.execution_slots),
        }
    }

    #[must_use]
    pub fn with_large_content_policy(
        client: Arc<GiteaClient>,
        token_client: impl Into<Option<Arc<TokenLifecycleClient>>>,
        policy: LargeContentPolicy,
    ) -> Self {
        // A ceiling below the floor cannot be honoured, so it is raised here
        // rather than quietly violated on every displaced reply.
        let policy = LargeContentPolicy {
            context_ceiling_bytes: policy
                .context_ceiling_bytes
                .max(MINIMUM_CONTEXT_CEILING_BYTES),
            ..policy
        };
        let budget = Arc::new(resources::ResourceBudget::new(
            policy.limits.max_total_bytes,
        ));
        Self {
            client,
            token_client: token_client.into(),
            policy,
            store: resources::ResourceStore::with_budget(policy.limits, Arc::clone(&budget)),
            budget,
            files: None,
            execution_slots: Arc::new(tokio::sync::Semaphore::new(8)),
        }
    }

    #[must_use]
    pub fn with_files(mut self, files: Option<Arc<files::FilePlane>>) -> Self {
        self.files = files;
        self
    }

    /// Set the shared execution capacity before creating client sessions.
    #[must_use]
    pub fn with_execution_limit(mut self, limit: usize) -> Self {
        self.execution_slots = Arc::new(tokio::sync::Semaphore::new(limit.clamp(1, 64)));
        self
    }

    fn execution_permit(&self) -> Result<tokio::sync::SemaphorePermit<'_>, McpError> {
        self.execution_slots.try_acquire().map_err(|_| {
            McpError::internal_error(
                "Server execution capacity is busy; no upstream request was sent",
                Some(json!({"code": "gitea_execution_busy", "outcome": "not_sent"})),
            )
        })
    }

    /// Turn one upstream response into a tool result.
    ///
    /// A failure is returned whole: error bodies are already held to a far
    /// tighter transport bound and carry the recovery detail a caller needs, so
    /// hiding one behind a URI would cost a round trip to learn why the call
    /// failed. Only successful payloads are subject to the context ceiling.
    fn operation_result(
        &self,
        response: gitea_api::OperationResponse,
        sensitive: bool,
        declared: Option<&str>,
        identity: Option<&OperationSpec>,
    ) -> Result<CallToolResult, McpError> {
        let meta = result_meta(sensitive, identity);
        if !response.success {
            let mut result =
                CallToolResult::structured(serde_json::to_value(response).map_err(|_| {
                    McpError::internal_error("failed to encode Gitea result", None)
                })?);
            result.is_error = Some(true);
            result.meta = meta;
            return Ok(result);
        }
        // The success path sets `is_error` and the metadata while fitting, so
        // that both are inside the measurement rather than added to a reply
        // that has already been sized.
        self.bound_response(&response, meta.as_ref(), declared)
    }

    /// Return a successful operation result, displacing its payload into the
    /// resource store when inlining it would flood the caller's context.
    fn bound_response(
        &self,
        response: &gitea_api::OperationResponse,
        meta: Option<&Meta>,
        declared: Option<&str>,
    ) -> Result<CallToolResult, McpError> {
        let encode_failed = || McpError::internal_error("failed to encode Gitea result", None);
        let sensitive = meta_is_sensitive(meta);

        // The ceiling is compared against the assembled tool result, because
        // that is what the transport serialises. Nothing smaller is a safe
        // proxy: base64 inflates binary by a third, JSON escaping expands
        // awkward text, the envelope carries declared headers, and
        // `CallToolResult::structured` additionally repeats the whole document
        // as an escaped text block — so the reply is well over twice the size of
        // the value handed to it.
        let (payload, media_type) = payload_bytes(response, declared).ok_or_else(encode_failed)?;
        let mut inline = CallToolResult::structured(
            serde_json::to_value(response).map_err(|_| encode_failed())?,
        );
        inline.is_error = Some(false);
        inline.meta = meta.cloned();
        if measured_len(&inline) <= self.policy.context_ceiling_bytes {
            return Ok(inline);
        }

        let payload_bytes_len = payload.len();
        let preview = text_preview(&response.data, &payload);
        let stored = self
            .store
            .insert(&response.operation_id, &media_type, sensitive, payload);

        let retention = self.retention(stored.as_ref(), payload_bytes_len);

        let summary = json!({
            "operation_id": response.operation_id,
            "status": response.status,
            "success": response.success,
            "content_type": response.content_type.as_deref().map(bounded_media_type),
            "headers": response.headers,
            "payload": retention,
        });
        let link = resource_link(stored.as_ref().ok(), payload_bytes_len);
        Ok(fit_reply_to_ceiling(
            summary,
            link.as_ref(),
            preview,
            meta,
            self.policy.context_ceiling_bytes,
        ))
    }

    /// How a displaced payload is accounted for, whether or not it was kept.
    ///
    /// Shared by every lane so that the two cannot drift: a caller must not be
    /// able to tell which branch produced a reply from the shape of its
    /// accounting.
    ///
    /// A failure to retain must never turn a completed upstream call into an
    /// error. The side effect has already happened, and an error telling the
    /// caller to retry would invite a second submission of a mutation that
    /// already succeeded. The result stays a success and says the body was not
    /// kept.
    fn retention(
        &self,
        stored: Result<&resources::StoredResource, &resources::StoreError>,
        payload_bytes_len: usize,
    ) -> Value {
        match stored {
            Ok(stored) => json!({
                "inlined": false,
                "retained": true,
                "reason": "above the context-scale ceiling",
                "bytes": payload_bytes_len,
                "context_ceiling_bytes": self.policy.context_ceiling_bytes,
                "resource_uri": stored.uri,
                "media_type": stored.content_type,
            }),
            Err(error) => {
                let detail = match error {
                    resources::StoreError::ObjectTooLarge { bytes, limit } => {
                        format!("{bytes} bytes exceeds the {limit}-byte resource store limit")
                    }
                    resources::StoreError::BudgetExhausted {
                        required,
                        available,
                    } => format!(
                        "retaining it needs {required} bytes of shared resource storage and \
                         {available} are free"
                    ),
                };
                json!({
                    "inlined": false,
                    "retained": false,
                    "reason": "above the context-scale ceiling and could not be retained",
                    "bytes": payload_bytes_len,
                    "context_ceiling_bytes": self.policy.context_ceiling_bytes,
                    "detail": detail,
                })
            }
        }
    }

    /// Fit a hand-written tool result to the context ceiling.
    ///
    /// The generated lane has its own entry point because it carries an upstream
    /// envelope — status, declared headers — that these results do not have.
    /// Below the ceiling both are identical, and above it both displace the same
    /// way through the same store and the same fitting step, so the ceiling is a
    /// property of the reply rather than of which branch built it.
    fn bound_value(
        &self,
        tool: &str,
        value: &Value,
        sensitive: bool,
    ) -> Result<CallToolResult, McpError> {
        let mut inline = CallToolResult::structured(value.clone());
        inline.is_error = Some(false);
        if sensitive {
            inline.meta = Some(Meta(Map::from_iter([(
                SENSITIVE_RESULT_META.to_string(),
                Value::Bool(true),
            )])));
        }
        if measured_len(&inline) <= self.policy.context_ceiling_bytes {
            return Ok(inline);
        }

        let payload = serde_json::to_vec(value)
            .map_err(|_| McpError::internal_error("failed to encode result", None))?;
        let payload_bytes_len = payload.len();
        let preview = text_preview(value, &payload);
        let stored = self
            .store
            .insert(tool, "application/json", sensitive, payload);
        let summary = json!({
            "tool": tool,
            "payload": self.retention(stored.as_ref(), payload_bytes_len),
        });
        Ok(fit_reply_to_ceiling(
            summary,
            resource_link(stored.as_ref().ok(), payload_bytes_len).as_ref(),
            preview,
            result_meta(sensitive, None).as_ref(),
            self.policy.context_ceiling_bytes,
        ))
    }

    /// Serve one resource by URI. The trait method is a delegation to this, so
    /// a test can exercise the wiring without constructing a request context.
    ///
    /// # Errors
    ///
    /// Returns a not-found error when the URI names no live payload.
    pub fn read_resource_uri(
        &self,
        uri: &str,
    ) -> Result<rmcp::model::ReadResourceResult, McpError> {
        let _permit = self.execution_permit()?;
        if uri == CATALOG_INDEX_URI {
            return Ok(rmcp::model::ReadResourceResult::new(vec![
                rmcp::model::ResourceContents::TextResourceContents {
                    uri: CATALOG_INDEX_URI.to_string(),
                    mime_type: Some("text/tab-separated-values".to_string()),
                    text: catalog_index(),
                    meta: None,
                },
            ]));
        }
        let stored = self
            .store
            .read(uri)
            .ok_or_else(|| McpError::resource_not_found(missing_resource_message(uri), None))?;
        Ok(rmcp::model::ReadResourceResult::new(vec![
            resource_contents(stored),
        ]))
    }

    /// One page of resources, the catalog index included on the cursorless page.
    #[must_use]
    pub fn list_resources_page(&self, cursor: Option<&str>) -> rmcp::model::ListResourcesResult {
        let (page, next_cursor) = self.store.page(cursor, RESOURCE_PAGE_SIZE);
        let mut resources = Vec::with_capacity(page.len() + 1);
        if lists_catalog_index(cursor) {
            resources.push(
                rmcp::model::Resource::new(CATALOG_INDEX_URI, "gitea operation index")
                    .with_description(
                        "every published catalog operation with its risk, domain, required \
                         arguments, and summary, one per line",
                    )
                    .with_mime_type("text/tab-separated-values"),
            );
        }
        resources.extend(page.into_iter().map(|stored| {
            let mut resource = rmcp::model::Resource::new(stored.uri, stored.operation_id.clone())
                .with_description(format!("payload from {}", stored.operation_id))
                .with_mime_type(stored.content_type);
            if stored.sensitive {
                resource.meta = Some(Meta(Map::from_iter([(
                    SENSITIVE_RESULT_META.to_string(),
                    Value::Bool(true),
                )])));
            }
            resource
        }));
        rmcp::model::ListResourcesResult {
            resources,
            next_cursor,
            meta: None,
        }
    }

    /// The published surface: the hand-written tools, the discovery pair, and
    /// the four execution lanes. Catalog operations are not published
    /// individually — they are selected through discovery and executed through
    /// a lane, which is what keeps their risk classification enforced rather
    /// than advisory.
    #[must_use]
    pub fn list_tools_payload() -> ListToolsResult {
        let mut tools = Vec::with_capacity(12);
        tools.push(server_version_tool());
        tools.extend(access_token_tools());
        tools.push(bootstrap::tool());
        tools.push(repository_secret::tool());
        tools.extend(discovery::tools());
        tools.extend(lanes::tools());
        let tools = tools
            .into_iter()
            .map(schema_portability::normalize_tool)
            .collect();
        ListToolsResult {
            tools,
            next_cursor: None,
            meta: None,
        }
    }

    /// Dispatch one registered tool.
    ///
    /// # Errors
    ///
    /// Returns an MCP error for an unknown tool, invalid arguments, or failed
    /// upstream operation.
    pub async fn invoke_tool(
        &self,
        params: CallToolRequestParams,
    ) -> Result<CallToolResult, McpError> {
        let _permit = self.execution_permit()?;
        if params.name == SERVER_VERSION_TOOL {
            if params
                .arguments
                .as_ref()
                .is_some_and(|arguments| !arguments.is_empty())
            {
                return Err(McpError::invalid_params(
                    "server.version does not accept arguments",
                    None,
                ));
            }
            let result = self.client.version().await;
            let result = match result {
                Ok(version) => version,
                Err(error) => return upstream_failure(&error),
            };
            return self.bound_value(
                SERVER_VERSION_TOOL,
                &json!({ "version": result.version }),
                false,
            );
        }
        if params.name == ACCESS_TOKEN_CREATE_TOOL {
            let arguments = parse_arguments::<CreateTokenArguments>(params.arguments)?;
            let token = match self
                .token_client()?
                .create(&CreateAccessToken {
                    name: arguments.name,
                    scopes: arguments.scopes,
                })
                .await
            {
                Ok(token) => token,
                Err(error) => return upstream_failure(&error),
            };
            let token = serde_json::to_value(token)
                .map_err(|_| McpError::internal_error("failed to encode token result", None))?;
            return self.bound_value(ACCESS_TOKEN_CREATE_TOOL, &token, true);
        }
        if params.name == ACCESS_TOKEN_LIST_TOOL {
            let arguments = parse_arguments::<ListTokenArguments>(params.arguments)?;
            let tokens = match self
                .token_client()?
                .list(arguments.page, arguments.limit)
                .await
            {
                Ok(tokens) => tokens,
                Err(error) => return upstream_failure(&error),
            };
            return self.bound_value(ACCESS_TOKEN_LIST_TOOL, &json!({ "tokens": tokens }), false);
        }
        if params.name == ACCESS_TOKEN_REVOKE_TOOL {
            RevokeTokenArguments::reject_null_selectors(params.arguments.as_ref())?;
            let arguments = parse_arguments::<RevokeTokenArguments>(params.arguments)?;
            let selector = arguments.into_selector()?;
            if let Err(error) = self.token_client()?.revoke(&selector).await {
                return upstream_failure(&error);
            }
            return self.bound_value(ACCESS_TOKEN_REVOKE_TOOL, &json!({ "revoked": true }), false);
        }
        if params.name == discovery::SEARCH_TOOL || params.name == discovery::DESCRIBE_TOOL {
            return self.discovery_result(params);
        }
        if params.name == bootstrap::TOOL_NAME {
            return self.bootstrap_result(params.arguments).await;
        }
        if params.name == repository_secret::TOOL_NAME {
            return self.repository_secret_result(params.arguments).await;
        }

        if let Some(lane) = lanes::Lane::from_tool_name(&params.name) {
            return self.lane_result(lane, params).await;
        }

        // A catalog operation is executed through its lane, never as a tool of
        // its own. Accepting the operation name here as well would leave lane
        // routing bypassable, so the name is refused — but it is refused with
        // the lane to call, because a caller holding a name from an older
        // surface deserves the one-line migration rather than a bare miss.
        if let Some(operation) = exposed_operation(&params.name) {
            let lane = lanes::lane_of(operation);
            return Err(McpError::invalid_params(
                format!(
                    "{} is a catalog operation, not a tool; call {} with operation_id {}",
                    params.name,
                    lane.tool_name(),
                    operation.tool_name
                ),
                None,
            ));
        }
        Err(McpError::method_not_found::<
            rmcp::model::CallToolRequestMethod,
        >())
    }

    fn token_client(&self) -> Result<&TokenLifecycleClient, McpError> {
        self.token_client.as_deref().ok_or_else(|| McpError::internal_error(
            "Token administration is unavailable: configure GITEA_MCP_TOKEN_USERNAME and GITEA_MCP_TOKEN_PASSWORD; no upstream request was sent",
            None,
        ))
    }

    async fn bootstrap_result(
        &self,
        raw_arguments: Option<Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let raw_arguments = raw_arguments.unwrap_or_default();
        bootstrap::validate_input(&raw_arguments)
            .map_err(|message| McpError::invalid_params(message, None))?;
        let arguments = bootstrap::parse(Some(raw_arguments))
            .map_err(|message| McpError::invalid_params(message, None))?;
        bootstrap::validate(&arguments)
            .map_err(|message| McpError::invalid_params(message, None))?;
        if arguments.requires_token_administration() {
            self.token_client()?;
        }
        let result =
            bootstrap::execute(&self.client, self.token_client.as_deref(), arguments).await;
        let is_error = !result.is_complete();
        let result = serde_json::to_value(result).map_err(|_| {
            McpError::internal_error("failed to encode repository bootstrap result", None)
        })?;
        let mut response = self.bound_value(bootstrap::TOOL_NAME, &result, true)?;
        // Set after fitting: a partial bootstrap is still a failure whether or
        // not its report had to be displaced.
        response.is_error = Some(is_error);
        Ok(response)
    }

    async fn repository_secret_result(
        &self,
        raw_arguments: Option<Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let arguments = parse_arguments::<repository_secret::Arguments>(raw_arguments)?;
        arguments
            .validate_target()
            .map_err(|message| McpError::invalid_params(message, None))?;
        for (name, value) in [
            ("owner", arguments.owner.as_str()),
            ("repo", arguments.repo.as_str()),
            ("secretname", arguments.name.as_str()),
        ] {
            gitea_api::validate_path_segment(name, value).map_err(|_| {
                McpError::invalid_params(format!("{name} is not a valid path segment"), None)
            })?;
        }
        let operation_arguments = Map::from_iter([
            ("owner".to_owned(), Value::String(arguments.owner)),
            ("repo".to_owned(), Value::String(arguments.repo)),
            ("secretname".to_owned(), Value::String(arguments.name)),
            (
                "body".to_owned(),
                json!({"data": "validated-secret-placeholder"}),
            ),
        ]);
        let operation = gitea_api::catalog::exposed_operation_by_id("updateRepoSecret")
            .ok_or_else(|| {
                McpError::internal_error("repository secret operation is absent", None)
            })?;
        // Validate the durable target before spending the single-use file
        // reference. The placeholder has the same schema shape as the secret,
        // while keeping the credential out of validation errors.
        validate_arguments(operation, &operation_arguments)?;
        let files = self.files.as_ref().ok_or_else(|| {
            McpError::invalid_params("repository secret file uploads are not configured", None)
        })?;
        let secret = files.take_secret(&arguments.data_file).map_err(|error| {
            McpError::invalid_params(error.to_string(), Some(json!({"code": error.code()})))
        })?;
        let mut response = self
            .client
            .execute_repository_secret(operation, operation_arguments, secret)
            .await
            .map_err(|error| {
                let mut mapped = map_operation_error(&error);
                mapped.data = Some(lanes::identity_error_data(operation, mapped.data.take()));
                mapped
            })?;
        // The workflow contract is status-only. Even an unexpected successful
        // upstream echo must not turn a credential input into output.
        response.data = Value::Null;
        self.operation_result(response, false, None, Some(operation))
    }

    /// Execute one catalog operation through a lane: same schema validation,
    /// same executor, and same ceilings as its flat tool, with the executed
    /// operation's identity inside the measured result metadata.
    async fn lane_result(
        &self,
        lane: lanes::Lane,
        params: CallToolRequestParams,
    ) -> Result<CallToolResult, McpError> {
        let arguments = parse_arguments::<lanes::LaneArguments>(params.arguments)?;
        let operation = lanes::resolve(lane, &arguments)?;
        validate_arguments(operation, &arguments.arguments)?;
        let response = self
            .client
            .execute_operation(operation, arguments.arguments)
            .await
            .map_err(|error| {
                // A failed lane call has no result to carry metadata, so the
                // resolved identity rides in the error data instead — an
                // outcome of sent_outcome_unknown or completed names an
                // operation that may have run.
                let mut mapped = map_operation_error(&error);
                mapped.data = Some(lanes::identity_error_data(operation, mapped.data.take()));
                mapped
            })?;
        self.operation_result(
            response,
            operation.secret_result,
            operation.produces.as_slice().first().map(String::as_str),
            Some(operation),
        )
    }

    /// Answer a discovery call from the registry, held to the same ceiling as
    /// every other reply.
    fn discovery_result(&self, params: CallToolRequestParams) -> Result<CallToolResult, McpError> {
        if params.name == discovery::SEARCH_TOOL {
            let arguments = parse_arguments::<discovery::SearchArguments>(params.arguments)?;
            let value = discovery::search(&arguments, &index_entries())?;
            return self.bound_value(discovery::SEARCH_TOOL, &value, false);
        }
        let arguments = parse_arguments::<discovery::DescribeArguments>(params.arguments)?;
        let value = discovery::describe(&arguments, &Self::list_tools_payload().tools)?;
        self.bound_value(discovery::DESCRIBE_TOOL, &value, false)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateTokenArguments {
    name: String,
    scopes: Vec<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListTokenArguments {
    page: Option<u32>,
    limit: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokeTokenArguments {
    id: Option<i64>,
    name: Option<String>,
}

impl RevokeTokenArguments {
    /// Refuse an explicitly null selector before it reads as an absent one.
    ///
    /// `null` and an omitted field both deserialize to `None`, so without this
    /// a call supplying two selectors would execute as a one-selector call and
    /// revoke a token the caller never meant to single out. The published
    /// schema types both fields, but nothing validates a hand-written tool's
    /// arguments against that schema before dispatch, so the rule holds here.
    fn reject_null_selectors(arguments: Option<&Map<String, Value>>) -> Result<(), McpError> {
        if arguments.is_some_and(|arguments| arguments.values().any(Value::is_null)) {
            return Err(McpError::invalid_params(
                "provide exactly one token id or name",
                None,
            ));
        }
        Ok(())
    }

    fn into_selector(self) -> Result<AccessTokenSelector, McpError> {
        match (self.id, self.name) {
            (Some(id), None) => Ok(AccessTokenSelector::Id(id)),
            (None, Some(name)) => Ok(AccessTokenSelector::Name(name)),
            _ => Err(McpError::invalid_params(
                "provide exactly one token id or name",
                None,
            )),
        }
    }
}

fn parse_arguments<T>(arguments: Option<Map<String, Value>>) -> Result<T, McpError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(Value::Object(arguments.unwrap_or_default()))
        .map_err(|_| McpError::invalid_params("arguments do not match the tool schema", None))
}

/// Report a failed generated operation with what is known about its outcome.
///
/// A bare failure is an instruction to try again, and for a mutation that is the
/// wrong instruction whenever the request may already have run. The message
/// therefore leads with whether the operation happened, and the same fact is
/// repeated in `data` so a caller can branch on it without parsing prose.
fn map_operation_error(error: &ApiError) -> McpError {
    let outcome = error.call_outcome();
    McpError::internal_error(
        format!("{error}; {}", retry_guidance(outcome)),
        Some(outcome_data(outcome)),
    )
}

/// What a caller should do next, given how far the call got.
///
/// One sentence covers both a refusal and an undeliverable success, because the
/// advice is the same: the call reached Gitea, so reissuing it unchanged either
/// repeats a side effect or earns the same refusal. Only the accompanying
/// detail differs.
const fn retry_guidance(outcome: CallOutcome) -> &'static str {
    match outcome {
        CallOutcome::Completed { .. } => {
            "the call reached Gitea and its outcome is settled, so do not reissue it unchanged"
        }
        CallOutcome::Unknown => {
            "the request was sent and no response arrived, so whether Gitea applied it is \
             unknown; reissuing it may repeat a side effect"
        }
        CallOutcome::NotSent => "the request was never sent, so nothing happened upstream",
    }
}

/// The same facts as [`retry_guidance`], for a caller that branches rather than
/// reads.
fn outcome_data(outcome: CallOutcome) -> Value {
    match outcome {
        CallOutcome::Completed { status } => {
            json!({"outcome": "completed", "status": status, "safe_to_retry": false})
        }
        CallOutcome::Unknown => json!({"outcome": "sent_outcome_unknown", "safe_to_retry": false}),
        CallOutcome::NotSent => json!({"outcome": "not_sent", "safe_to_retry": true}),
    }
}

/// Report a failed hand-written call.
///
/// Gitea refusing a call is not this server malfunctioning, so a refusal is a
/// tool result marked `is_error` — the same shape the generated lane produces —
/// rather than a protocol error. A protocol error says the request could not be
/// processed at all, which sends a caller looking for a fault in the wrong
/// place and hides a status that was perfectly well delivered.
///
/// Bad input stays a protocol error: the call was never valid to make.
///
/// The result carries no `structuredContent`, because the tool's declared
/// output schema describes a token or a token list and a refusal is neither.
fn upstream_failure(error: &ApiError) -> Result<CallToolResult, McpError> {
    if let ApiError::InvalidAccessTokenInput(message) = error {
        return Err(McpError::invalid_params((*message).to_string(), None));
    }
    let outcome = error.call_outcome();
    if outcome == CallOutcome::NotSent && !matches!(error, ApiError::Upstream { .. }) {
        // Nothing reached Gitea and nothing is wrong upstream: a configuration
        // or construction fault the caller cannot act on as a tool result.
        return Err(map_operation_error(error));
    }
    let mut result = CallToolResult::error(vec![ContentBlock::text(format!(
        "{error}; {}",
        retry_guidance(outcome)
    ))]);
    result.is_error = Some(true);
    result.meta = Some(Meta(Map::from_iter([(
        OUTCOME_META.to_string(),
        outcome_data(outcome),
    )])));
    Ok(result)
}

/// The payload an oversized response would have inlined, plus its media type.
///
/// A decoded string is stored as its own bytes rather than as JSON: the point of
/// the resource is that reading it yields the log or diff itself, not a
/// quote-escaped copy of one. Everything else round-trips as JSON.
fn payload_bytes(
    response: &gitea_api::OperationResponse,
    declared: Option<&str>,
) -> Option<(Vec<u8>, String)> {
    // `execute_operation` decodes by the response header when present and falls
    // back to the operation's declared `produces`. Classifying here without that
    // fallback would read a headerless JSON body as binary.
    let effective = response.content_type.as_deref().or(declared).unwrap_or("");
    match &response.data {
        // A decoded string is stored as its own bytes so that reading the
        // resource yields the log or diff itself. That is only right when the
        // media type says text: a JSON body that happens to decode to a string
        // is still JSON, and storing it unquoted would label invalid JSON as
        // `application/json`.
        // Labelled with the same effective type used to decode it. Reading the
        // header alone would file a headerless text/html response as text/plain,
        // discarding the declared type that decoding had already relied on.
        Value::String(text) if !declares_json(effective) => Some((
            text.as_bytes().to_vec(),
            bounded_media_type(if effective.is_empty() {
                "text/plain"
            } else {
                effective
            }),
        )),
        // A declared binary response arrives base64-encoded inside a JSON
        // envelope, which inflates it by a third. Storing that expansion would
        // put an artifact the transport accepted above the store's object cap
        // purely because of the encoding, and would hand back an envelope
        // rather than the file. The original bytes are restored instead.
        //
        // Gated on the declared media type, mirroring the only condition under
        // which `gitea-api` synthesises this wrapper: a body that is neither
        // JSON nor text. Shape alone is not enough — `GitBlobResponse` and
        // `ContentsResponse` both declare `encoding` and `content`, and Gitea
        // omits empty fields, so a blob can serialise to exactly those two keys
        // while being a real JSON response whose other fields matter.
        Value::Object(fields)
            if !is_textual_media_type(effective)
                && fields.len() == 2
                && fields.get("encoding").and_then(Value::as_str) == Some("base64")
                && fields.contains_key("content") =>
        {
            let encoded = fields.get("content").and_then(Value::as_str)?;
            Some((
                BASE64_STANDARD.decode(encoded).ok()?,
                bounded_media_type(
                    response
                        .content_type
                        .as_deref()
                        .unwrap_or("application/octet-stream"),
                ),
            ))
        }
        other => Some((
            serde_json::to_vec(other).ok()?,
            "application/json".to_string(),
        )),
    }
}

/// A leading slice of a textual payload, cut on a character boundary.
fn text_preview(data: &Value, payload: &[u8]) -> Option<String> {
    if !data.is_string() {
        return None;
    }
    let text = std::str::from_utf8(payload).ok()?;
    if text.len() <= RESOURCE_PREVIEW_BYTES {
        return Some(text.to_string());
    }
    let mut end = RESOURCE_PREVIEW_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    Some(text[..end].to_string())
}

/// Render a stored payload as MCP resource contents.
///
/// Text is returned as text and anything else as a blob, so a caller reading a
/// log gets the log rather than an encoded copy of it. The sensitivity
/// classification travels with the payload, not with the reply that displaced
/// it: a gateway inspecting only this response would otherwise see a
/// credential-bearing body with nothing marking it as one.
/// What to say when a handle names no live payload.
///
/// Deliberately does not direct a re-run. Every generated operation can displace
/// a payload, destructive ones included, and the call that produced the handle
/// already completed; telling a caller to repeat it would invite a second side
/// effect in order to recover a copy of the first one's result.
/// The metadata a call result carries: the sensitivity marker when the result
/// is sensitive, plus the executed operation's identity when the call arrived
/// through a lane. Built before any measurement so nothing rides outside the
/// ceiling.
fn result_meta(sensitive: bool, identity: Option<&OperationSpec>) -> Option<Meta> {
    let mut fields = Map::new();
    if sensitive {
        fields.insert(SENSITIVE_RESULT_META.to_string(), Value::Bool(true));
    }
    if let Some(operation) = identity {
        return Some(lanes::identity_meta(operation, Some(Meta(fields))));
    }
    if fields.is_empty() {
        return None;
    }
    Some(Meta(fields))
}

/// Whether metadata carries the sensitivity marker.
fn meta_is_sensitive(meta: Option<&Meta>) -> bool {
    meta.and_then(|meta| meta.0.get(SENSITIVE_RESULT_META))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Serialized size of a value, as the transport will send it.
fn measured_len<T: serde::Serialize>(value: &T) -> usize {
    // An unserializable reply is treated as unboundedly large so that shedding
    // still runs; it cannot be returned to a caller either way.
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
}

/// Assemble the replacement reply and hold it to `ceiling`.
///
/// Convenience is shed under pressure, largest first: the excerpt, then the
/// declared headers. The handle is never shed, since carrying it is the reply's
/// whole purpose, and `headers_omitted` records the loss so a caller comparing
/// against an inline result does not find headers silently missing.
///
/// Every decision is made by measuring the assembled `CallToolResult`, because
/// that is what the transport serialises: it repeats the whole document as an
/// escaped text block and carries the resource link, so the structured member
/// alone understates it by more than half.
/// The resource link a displaced reply carries, when the payload was kept.
fn resource_link(
    stored: Option<&resources::StoredResource>,
    payload_bytes_len: usize,
) -> Option<rmcp::model::Resource> {
    stored.map(|stored| {
        rmcp::model::Resource::new(stored.uri.clone(), stored.operation_id.clone())
            .with_description(format!(
                "{payload_bytes_len}-byte {} payload from {}, held until it expires",
                stored.content_type, stored.operation_id
            ))
            .with_mime_type(stored.content_type.clone())
    })
}

fn fit_reply_to_ceiling(
    mut summary: Value,
    link: Option<&rmcp::model::Resource>,
    preview: Option<String>,
    meta: Option<&Meta>,
    ceiling: usize,
) -> CallToolResult {
    // Every field the caller will actually receive is present before anything is
    // measured. `is_error` and the sensitivity metadata used to be attached by
    // the caller after fitting, which put them outside every size decision — and
    // since the excerpt search consumes exactly the space that remains, anything
    // added afterwards pushed the reply back over the ceiling.
    let assemble = |summary: &Value| {
        let mut result = CallToolResult::structured(summary.clone());
        if let Some(link) = link.cloned() {
            result
                .content
                .push(rmcp::model::ContentBlock::ResourceLink(link));
        }
        result.is_error = Some(false);
        result.meta = meta.cloned();
        result
    };

    // Convenience is shed under pressure, largest first: the excerpt, then the
    // declared headers. The handle is never shed — carrying it is the reply's
    // whole purpose.
    if let Some(preview) = preview
        && summary["payload"]["retained"] == Value::Bool(true)
    {
        // The largest prefix that still fits, found by measuring rather than by
        // estimating. A byte added to the summary costs more than a byte in the
        // reply, since the document is repeated as an escaped text block, and
        // the multiplier depends on the content — so arithmetic here would be a
        // guess.
        // Searched over character boundaries rather than byte offsets. Snapping
        // a byte midpoint backwards onto a boundary can land below the low bound
        // inside a multibyte scalar, leaving the bound unchanged and the search
        // running forever — a non-ASCII log would pin a worker after the
        // upstream call had already succeeded.
        let boundaries: Vec<usize> = preview
            .char_indices()
            .map(|(index, _)| index)
            .chain(std::iter::once(preview.len()))
            .filter(|index| *index > 0 && *index <= RESOURCE_PREVIEW_BYTES)
            .collect();
        let mut low = 0;
        let mut high = boundaries.len();
        let mut best: Option<String> = None;
        while low < high {
            let middle = usize::midpoint(low, high);
            let end = boundaries[middle];
            let mut candidate = summary.clone();
            if let Some(entry) = candidate.get_mut("payload").and_then(Value::as_object_mut) {
                entry.insert(
                    "preview".to_string(),
                    Value::String(preview[..end].to_string()),
                );
            }
            if measured_len(&assemble(&candidate)) <= ceiling {
                best = Some(preview[..end].to_string());
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        if let Some(best) = best
            && let Some(entry) = summary.get_mut("payload").and_then(Value::as_object_mut)
        {
            entry.insert("preview".to_string(), Value::String(best));
        }
    }
    if measured_len(&assemble(&summary)) > ceiling {
        if let Some(headers) = summary.get_mut("headers") {
            *headers = json!({});
        }
        if let Some(entry) = summary.get_mut("payload").and_then(Value::as_object_mut) {
            entry.insert("headers_omitted".to_string(), Value::Bool(true));
        }
    }
    assemble(&summary)
}

/// A line-per-operation index of the exposed catalog.
///
/// Served as a resource that costs nothing until it is asked for, and mirrored
/// by `catalog.search`, which filters the same rows, and `catalog.describe`,
/// which expands one of them. All three derive from the registry; deprecated
/// operations appear in none of them, though exact-name describe and dispatch
/// still answer for one.
///
/// One line per operation: name, risk, domain, required arguments, summary.
/// Tab-separated so it can be split without a parser, summary last because it
/// is the only field that contains spaces.
fn catalog_index() -> String {
    let mut lines = String::with_capacity(64 * 1024);
    lines.push_str("# tool\trisk\tdomain\trequired\tsummary\n");
    for entry in index_entries() {
        let _ = writeln!(
            lines,
            "{}\t{}\t{}\t{}\t{}",
            entry.tool, entry.risk, entry.domain, entry.required, entry.summary,
        );
    }
    lines
}

pub(crate) struct IndexEntry {
    pub(crate) tool: String,
    pub(crate) risk: &'static str,
    pub(crate) domain: String,
    pub(crate) required: String,
    pub(crate) summary: String,
    /// From the published metadata channel, so hand-written replacements keep
    /// their upstream identity. Absent for tools with no upstream operation.
    pub(crate) operation_id: Option<String>,
    pub(crate) administrative: bool,
    pub(crate) sensitive: bool,
}

/// One entry per registered tool, derived from the registry itself.
///
/// Risk and required arguments come from the published `Tool` — its annotations
/// and its input schema — rather than from a table maintained beside it. A
/// hand-maintained table drifts silently: it labelled `repository.bootstrap` a
/// mutation while the tool it describes is annotated destructive, and omitted
/// required arguments that the schema demands. Deriving from the same object a
/// caller receives makes that disagreement impossible rather than merely
/// tested-for. The operation id, administrative flag, and sensitive-result
/// flag come from the same object's metadata channel for the same reason: a
/// generated-catalog lookup misses the hand-written tools, which are exactly
/// the credential-bearing ones a misclassification matters for.
///
/// The summary still comes from the generated catalog where there is one, since
/// a tool's description carries selection guidance and a risk sentence that
/// would bloat every line.
///
/// The required column names what a caller cannot omit. A schema states that in
/// two ways: unconditionally in `required`, and as a choice in a `oneOf` whose
/// branches each require a different argument. A tool with only the second form
/// is still uncallable without one of them, so reading `required` alone would
/// print an empty column for a call that cannot be made — worse than saying
/// nothing, because the index is what a caller consults instead of the schema.
/// The choice is rendered `a|b` to distinguish it from the comma-joined
/// conjunction.
pub(crate) fn required_arguments(input_schema: &Map<String, Value>) -> String {
    let names = |value: Option<&Value>| -> Vec<String> {
        value
            .and_then(Value::as_array)
            .map(|names| {
                names
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };

    let mut columns = names(input_schema.get("required"));
    if let Some(branches) = input_schema.get("oneOf").and_then(Value::as_array) {
        let choice: Vec<String> = branches
            .iter()
            .filter_map(|branch| {
                let branch_required = names(branch.get("required"));
                (!branch_required.is_empty()).then(|| branch_required.join(","))
            })
            .collect();
        if !choice.is_empty() {
            columns.push(choice.join("|"));
        }
    }
    columns.join(",")
}

pub(crate) fn index_entries() -> Vec<IndexEntry> {
    // Two sources, one rule: each row is derived from the object that defines
    // it. A published tool describes itself through its annotations, schema,
    // and metadata; a catalog operation describes itself through the same
    // generated spec the lanes validate against. Nothing is maintained beside
    // them, so no row can disagree with what a call will actually do.
    //
    // The lanes are excluded deliberately: the index is a catalog of
    // operations to execute, and a lane is how one is executed, not one of
    // them.
    let mut entries: Vec<IndexEntry> = GiteaMcp::list_tools_payload()
        .tools
        .into_iter()
        .filter(|tool| lanes::Lane::from_tool_name(&tool.name).is_none())
        .map(|tool| {
            let name = tool.name.to_string();
            let annotations = tool.annotations.as_ref();
            let risk = if annotations.and_then(|a| a.destructive_hint) == Some(true) {
                "destructive"
            } else if annotations.and_then(|a| a.read_only_hint) == Some(true) {
                "read"
            } else {
                "mutation"
            };
            let meta = tool.meta.as_ref();
            let meta_bool = |key: &str| {
                meta.and_then(|meta| meta.0.get(key))
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            };
            IndexEntry {
                operation_id: meta
                    .and_then(|meta| meta.0.get("org.cacahuate/operationId"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                administrative: meta_bool("org.cacahuate/administrative"),
                sensitive: meta_bool(SENSITIVE_RESULT_META),
                domain: name
                    .split_once('.')
                    .map_or_else(|| "server".to_string(), |(domain, _)| domain.to_string()),
                required: required_arguments(&tool.input_schema),
                summary: tool.description.as_deref().unwrap_or_default().to_string(),
                tool: name,
                risk,
            }
        })
        .collect();

    entries.extend(
        operation_catalog()
            .operations
            .iter()
            .filter(|operation| operation.exposed && !operation.deprecated.is_deprecated())
            .map(|operation| IndexEntry {
                operation_id: Some(operation.operation_id.clone()),
                administrative: operation.administrative,
                sensitive: operation.secret_result,
                domain: operation
                    .tool_name
                    .split_once('.')
                    .map_or_else(|| "server".to_string(), |(domain, _)| domain.to_string()),
                tool: operation.tool_name.clone(),
                risk: match operation.risk {
                    OperationRisk::Read => "read",
                    OperationRisk::Mutation => "mutation",
                    OperationRisk::Destructive => "destructive",
                },
                required: required_arguments(&operation.input_schema),
                summary: operation.summary.clone(),
            }),
    );
    entries
}

/// Whether a `resources/list` page carries the catalog index.
///
/// The cursorless page only. Keying on "no next cursor" as well would put it on
/// the terminal page too, repeating it once per traversal.
const fn lists_catalog_index(cursor: Option<&str>) -> bool {
    cursor.is_none()
}

fn missing_resource_message(uri: &str) -> String {
    format!(
        "no stored payload at {uri}; retained payloads expire and may be evicted. \
         Repeating the operation that produced it would repeat any side effect it had."
    )
}

fn resource_contents(stored: resources::StoredResource) -> rmcp::model::ResourceContents {
    let meta = stored.sensitive.then(|| {
        Meta(Map::from_iter([(
            SENSITIVE_RESULT_META.to_string(),
            Value::Bool(true),
        )]))
    });
    // Representation follows the declared media type, not whether the bytes
    // happen to decode. An archive or an image can be accidentally valid UTF-8,
    // and returning one as text would present a caller with mojibake in place of
    // the file it asked for.
    let as_text = is_textual_media_type(&stored.content_type)
        .then(|| String::from_utf8(stored.body.clone()).ok())
        .flatten();
    match as_text {
        Some(text) => rmcp::model::ResourceContents::TextResourceContents {
            uri: stored.uri,
            mime_type: Some(stored.content_type),
            text,
            meta,
        },
        None => rmcp::model::ResourceContents::BlobResourceContents {
            uri: stored.uri,
            mime_type: Some(stored.content_type),
            blob: BASE64_STANDARD.encode(stored.body),
            meta,
        },
    }
}

/// Retained payloads named in one `resources/list` page.
const RESOURCE_PAGE_SIZE: usize = 50;

/// Longest media type carried into a reply or a stored resource.
///
/// Upstream copies the response `Content-Type` without a length bound, so a
/// pathological value would otherwise appear in the summary, the resource
/// metadata, and the link description at once, defeating the ceiling from three
/// directions. A media type is a label; anything past this is not one.
const MAX_MEDIA_TYPE_BYTES: usize = 128;

/// Truncate a media type to [`MAX_MEDIA_TYPE_BYTES`] on a character boundary.
fn bounded_media_type(media_type: &str) -> String {
    if media_type.len() <= MAX_MEDIA_TYPE_BYTES {
        return media_type.to_string();
    }
    let mut end = MAX_MEDIA_TYPE_BYTES;
    while end > 0 && !media_type.is_char_boundary(end) {
        end -= 1;
    }
    media_type[..end].to_string()
}

/// Whether a media type denotes JSON, whose serialized form must be preserved.
fn declares_json(media_type: &str) -> bool {
    let normalized = media_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    normalized == "application/json" || normalized.ends_with("+json")
}

/// Whether a media type denotes text a caller should read directly.
fn is_textual_media_type(media_type: &str) -> bool {
    let normalized = media_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    normalized.starts_with("text/")
        || normalized.ends_with("+json")
        || normalized.ends_with("+xml")
        || matches!(
            normalized.as_str(),
            "application/json" | "application/xml" | "application/x-ndjson"
        )
}

fn access_token_tools() -> [Tool; 3] {
    [
        access_token_create_tool(),
        access_token_list_tool(),
        access_token_revoke_tool(),
    ]
}

fn access_token_create_tool() -> Tool {
    let mut tool = Tool::new(
        Cow::Borrowed(ACCESS_TOKEN_CREATE_TOOL),
        Cow::Borrowed(
            "Create a scoped Gitea access token. The token value is returned once as a sensitive result.",
        ),
        Arc::new(json_object_schema(
            json!({
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
                    "items": {
                        "type": "string",
                        "enum": token_scopes()
                    }
                }
            }),
            &["name", "scopes"],
        )),
    )
    .with_annotations(
        ToolAnnotations::new()
            .read_only(false)
            .destructive(false)
            .idempotent(false)
            .open_world(false),
    );
    tool.output_schema = Some(Arc::new(displaceable_output_schema(
        access_token_created_properties(),
        ACCESS_TOKEN_CREATED_REQUIRED,
    )));
    tool.meta = Some(tool_meta("userCreateToken", "mutation", true));
    tool
}

/// The shape of a created token.
///
/// Used twice, and the two uses must not be conflated: as this tool's own
/// output, where the whole reply can be displaced above the ceiling, and nested
/// inside the bootstrap report, where it cannot — displacement replaces an
/// entire result, never a field within one. Wrapping the nested use in the
/// displaceable union would advertise a shape the runtime never produces there.
pub(crate) const ACCESS_TOKEN_CREATED_REQUIRED: &[&str] = &[
    "id",
    "name",
    "scopes",
    "token",
    "token_last_eight",
    "created_at",
];

pub(crate) fn access_token_created_properties() -> Value {
    json!({
        "id": {"type": "integer"},
        "name": {"type": "string"},
        "scopes": {"type": "array", "items": {"type": "string"}},
        "token": {"type": "string"},
        "token_last_eight": {"type": "string"},
        "created_at": {"type": ["string", "null"]}
    })
}

/// The nested form: no displaced alternative.
pub(crate) fn access_token_created_schema() -> Map<String, Value> {
    json_object_schema(
        access_token_created_properties(),
        ACCESS_TOKEN_CREATED_REQUIRED,
    )
}

fn access_token_list_tool() -> Tool {
    let mut tool = Tool::new(
        Cow::Borrowed(ACCESS_TOKEN_LIST_TOOL),
        Cow::Borrowed("List access-token metadata without returning token values."),
        Arc::new(json_object_schema(
            json!({
                "page": {"type": "integer", "minimum": 1},
                "limit": {"type": "integer", "minimum": 1, "maximum": 100}
            }),
            &[],
        )),
    )
    .with_annotations(
        ToolAnnotations::new()
            .read_only(true)
            .destructive(false)
            .idempotent(true)
            .open_world(false),
    );
    tool.output_schema = Some(Arc::new(displaceable_output_schema(
        json!({
            "tokens": {
                "type": "array",
                "maxItems": 100,
                "items": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "integer"},
                        "name": {"type": "string"},
                        "scopes": {"type": "array", "items": {"type": "string"}},
                        "token_last_eight": {"type": "string"},
                        "created_at": {"type": ["string", "null"]},
                        "last_used_at": {"type": ["string", "null"]}
                    },
                    "required": [
                        "id", "name", "scopes", "token_last_eight", "created_at", "last_used_at"
                    ],
                    "additionalProperties": false
                }
            }
        }),
        &["tokens"],
    )));
    tool.meta = Some(tool_meta("userGetTokens", "read", false));
    tool
}

fn access_token_revoke_tool() -> Tool {
    // Exactly one of `id` or `name` is required, and that choice is enforced by
    // `RevokeTokenArguments::into_selector` rather than stated here as a
    // root-level `oneOf`. A client whose tool definitions must satisfy the
    // Anthropic API rejects an input schema with a root-level combinator and
    // drops the offending tool from its catalog, so declaring the choice in the
    // schema costs the whole revoke capability on those clients. Validating one
    // layer later keeps the tool reachable and still refuses a call that names
    // neither selector or both. Nesting a combinator under a property is
    // accepted, but that would rename the arguments callers already use.
    let input_schema = json_object_schema(
        json!({
            "id": {"type": "integer", "minimum": 1},
            "name": {
                "type": "string",
                "minLength": 1,
                "maxLength": 255,
                "pattern": "^(?![+-]?[0-9]+$).+$"
            }
        }),
        &[],
    );
    let mut tool = Tool::new(
        Cow::Borrowed(ACCESS_TOKEN_REVOKE_TOOL),
        Cow::Borrowed("Revoke an access token by exactly one explicit Gitea name or numeric ID."),
        Arc::new(input_schema),
    )
    .with_annotations(
        ToolAnnotations::new()
            .read_only(false)
            .destructive(true)
            .idempotent(true)
            .open_world(false),
    );
    tool.output_schema = Some(Arc::new(displaceable_output_schema(
        json!({"revoked": {"type": "boolean"}}),
        &["revoked"],
    )));
    tool.meta = Some(tool_meta("userDeleteAccessToken", "destructive", false));
    tool
}

pub(crate) fn json_object_schema(properties: Value, required: &[&str]) -> Map<String, Value> {
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

/// A hand-written tool's output schema, admitting a displaced reply.
///
/// Every result can be displaced above the context ceiling, so every output
/// schema has to describe both shapes or a large reply would violate the
/// contract its tool advertises. Declared the way the generated schema declares
/// it: one object type, the union of both property sets, and a `oneOf` over
/// which set is required.
///
/// Two of these tools cannot realistically reach the ceiling — a single token, a
/// version string — and are declared alike anyway. A caller should not have to
/// know which tools can displace, and the exception is what would rot.
///
/// Separate from [`json_object_schema`] because that also builds *input*
/// schemas, where there is nothing to displace and an empty `required` would
/// make the `oneOf` invalid.
pub(crate) fn displaceable_output_schema(
    properties: Value,
    required: &[&str],
) -> Map<String, Value> {
    let mut properties = properties;
    if let Some(map) = properties.as_object_mut() {
        map.insert("tool".to_string(), json!({"type": "string"}));
        map.insert("payload".to_string(), displaced_payload_schema());
    }
    Map::from_iter([
        ("type".to_string(), Value::String("object".to_string())),
        ("properties".to_string(), properties),
        ("additionalProperties".to_string(), Value::Bool(false)),
        // Exactly one of the two: the result itself, or an account of where it
        // went. Both optional would let a reply carrying neither pass.
        (
            "oneOf".to_string(),
            json!([
                {"required": required},
                {"required": ["tool", "payload"]}
            ]),
        ),
    ])
}

/// How a displaced payload is described, wherever it appears.
fn displaced_payload_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "inlined": {"type": "boolean"},
            "retained": {"type": "boolean"},
            "reason": {"type": "string"},
            "detail": {"type": "string"},
            "bytes": {"type": "integer"},
            "context_ceiling_bytes": {"type": "integer"},
            "resource_uri": {"type": "string"},
            "media_type": {"type": "string"},
            "preview": {"type": "string"},
            "headers_omitted": {"type": "boolean"}
        },
        "required": [
            "inlined",
            "retained",
            "reason",
            "bytes",
            "context_ceiling_bytes"
        ],
        "additionalProperties": false
    })
}

fn tool_meta(operation_id: &str, risk: &str, sensitive: bool) -> Meta {
    Meta(Map::from_iter([
        (
            "org.cacahuate/operationId".to_string(),
            Value::String(operation_id.to_string()),
        ),
        (
            "org.cacahuate/administrative".to_string(),
            Value::Bool(true),
        ),
        (
            "org.cacahuate/risk".to_string(),
            Value::String(risk.to_string()),
        ),
        (SENSITIVE_RESULT_META.to_string(), Value::Bool(sensitive)),
    ]))
}

fn token_scopes() -> Vec<String> {
    let mut scopes = vec!["all".to_string()];
    for access in ["read", "write"] {
        for domain in [
            "activitypub",
            "admin",
            "issue",
            "misc",
            "notification",
            "organization",
            "package",
            "repository",
            "user",
        ] {
            scopes.push(format!("{access}:{domain}"));
        }
    }
    scopes
}

fn server_version_tool() -> Tool {
    let input_schema = Map::from_iter([
        ("type".to_string(), Value::String("object".to_string())),
        ("properties".to_string(), Value::Object(Map::new())),
        ("additionalProperties".to_string(), Value::Bool(false)),
    ]);
    let output_schema = server_version_output_schema();
    let mut tool = Tool::new(
        Cow::Borrowed(SERVER_VERSION_TOOL),
        Cow::Borrowed("Return the upstream Gitea version."),
        Arc::new(input_schema),
    )
    .with_annotations(
        ToolAnnotations::new()
            .read_only(true)
            .destructive(false)
            .idempotent(true)
            .open_world(false),
    );
    tool.output_schema = Some(Arc::new(output_schema));
    // The upstream operation this tool replaces, so discovery resolves the
    // ledger's `getVersion` mapping to it like every other replacement.
    tool.meta = Some(Meta(Map::from_iter([
        (
            "org.cacahuate/operationId".to_string(),
            Value::String("getVersion".to_string()),
        ),
        (
            "org.cacahuate/risk".to_string(),
            Value::String("read".to_string()),
        ),
        (
            "org.cacahuate/administrative".to_string(),
            Value::Bool(false),
        ),
        (SENSITIVE_RESULT_META.to_string(), Value::Bool(false)),
    ])));
    tool
}

fn server_version_output_schema() -> Map<String, Value> {
    displaceable_output_schema(json!({"version": {"type": "string"}}), &["version"])
}

/// Result shape shared by every generated tool.
///
/// A successful result carries either its payload in `data` or, when the payload
/// was above the context-scale ceiling, a `payload` handle naming where it went.
/// Exactly one of the two is present: declaring both optional would let a result
/// satisfy the schema while carrying neither.
pub(crate) fn generated_output_schema() -> Map<String, Value> {
    Map::from_iter([
        ("type".to_string(), Value::String("object".to_string())),
        (
            "properties".to_string(),
            json!({
                "operation_id": {"type": "string"},
                "status": {"type": "integer"},
                "success": {"type": "boolean"},
                "content_type": {"type": ["string", "null"]},
                "headers": {
                    "type": "object",
                    "additionalProperties": {
                        "oneOf": [
                            {"type": "string"},
                            {"type": "array", "items": {"type": "string"}}
                        ]
                    }
                },
                "data": {},
                "payload": {
                    "type": "object",
                    "properties": {
                        "inlined": {"type": "boolean"},
                        "retained": {"type": "boolean"},
                        "reason": {"type": "string"},
                        "detail": {"type": "string"},
                        "bytes": {"type": "integer"},
                        "context_ceiling_bytes": {"type": "integer"},
                        "resource_uri": {"type": "string"},
                        "media_type": {"type": "string"},
                        "preview": {"type": "string"},
                        "headers_omitted": {"type": "boolean"}
                    },
                    "required": [
                        "inlined",
                        "retained",
                        "reason",
                        "bytes",
                        "context_ceiling_bytes"
                    ],
                    "additionalProperties": false
                }
            }),
        ),
        (
            "required".to_string(),
            json!([
                "operation_id",
                "status",
                "success",
                "content_type",
                "headers"
            ]),
        ),
        // Exactly one of the two: a result either carries its payload or says
        // where the payload went. Declaring both optional would let a malformed
        // result satisfy the schema while carrying neither.
        (
            "oneOf".to_string(),
            json!([{"required": ["data"]}, {"required": ["payload"]}]),
        ),
        ("additionalProperties".to_string(), Value::Bool(false)),
    ])
}

fn validate_arguments(
    operation: &OperationSpec,
    arguments: &Map<String, Value>,
) -> Result<(), McpError> {
    let schema = Value::Object(operation.input_schema.clone());
    let validator = jsonschema::draft4::options()
        .build(&schema)
        .map_err(|_| McpError::internal_error("generated tool schema is invalid", None))?;
    let instance = Value::Object(arguments.clone());
    validator.validate(&instance).map_err(|error| {
        let path = error.instance_path().as_str();
        McpError::invalid_params(
            if path.is_empty() {
                "arguments do not match the generated Gitea schema".to_string()
            } else {
                format!("arguments do not match the generated Gitea schema at {path}")
            },
            None,
        )
    })
}

impl ServerHandler for GiteaMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_protocol_version(ProtocolVersion::V_2025_11_25)
        .with_server_info(Implementation::new(
            "mcp-gitea-rs",
            env!("CARGO_PKG_VERSION"),
        ))
        .with_instructions(SERVER_INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _params: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let _permit = self.execution_permit()?;
        Ok(Self::list_tools_payload())
    }

    async fn list_resources(
        &self,
        params: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ListResourcesResult, McpError> {
        let _permit = self.execution_permit()?;
        Ok(self.list_resources_page(params.as_ref().and_then(|params| params.cursor.as_deref())))
    }

    async fn read_resource(
        &self,
        params: rmcp::model::ReadResourceRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ReadResourceResult, McpError> {
        self.read_resource_uri(&params.uri)
    }

    async fn call_tool(
        &self,
        params: CallToolRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke_tool(params).await
    }

    async fn on_custom_request(
        &self,
        request: rmcp::model::CustomRequest,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CustomResult, McpError> {
        let _permit = self.execution_permit()?;
        if request.method != files::AUTHORIZE_UPLOAD {
            return Err(McpError::new(
                rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                request.method,
                None,
            ));
        }
        let files = self.files.as_ref().ok_or_else(|| {
            McpError::new(
                rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                request.method.clone(),
                None,
            )
        })?;
        let params = request
            .params
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| McpError::invalid_params("invalid file upload authorization", None))?
            .unwrap_or_default();
        let result = files
            .authorize_upload(params)
            .map_err(|error| file_authorization_error(&error))?;
        Ok(rmcp::model::CustomResult::new(
            serde_json::to_value(result).map_err(|_| {
                McpError::internal_error("failed to encode file authorization", None)
            })?,
        ))
    }
}

fn file_authorization_error(error: &files::FileError) -> McpError {
    let data = Some(json!({"code": error.code()}));
    match error {
        files::FileError::TooManyOutstanding | files::FileError::EntropyUnavailable => {
            McpError::internal_error(error.to_string(), data)
        }
        _ => McpError::invalid_params(error.to_string(), data),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn small_policy(ceiling: usize) -> LargeContentPolicy {
        LargeContentPolicy {
            context_ceiling_bytes: ceiling,
            limits: resources::ResourceLimits {
                max_object_bytes: 1024 * 1024,
                max_total_bytes: 1024 * 1024,
                time_to_live: Duration::from_mins(1),
            },
        }
    }

    fn bounded_mcp(policy: LargeContentPolicy) -> GiteaMcp {
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
        GiteaMcp::with_large_content_policy(client, token_client, policy)
    }

    fn response(data: Value, content_type: &str) -> gitea_api::OperationResponse {
        gitea_api::OperationResponse {
            operation_id: "repoDownloadActionsRunJobLogs".to_string(),
            status: 200,
            success: true,
            content_type: Some(content_type.to_string()),
            headers: Map::new(),
            data,
        }
    }

    fn test_mcp(client: Arc<GiteaClient>) -> GiteaMcp {
        let token_client = Arc::new(
            TokenLifecycleClient::new(
                "https://gitea.example.test",
                "token-user",
                "token-password",
                Duration::from_secs(1),
            )
            .expect("token client"),
        );
        GiteaMcp::new(client, token_client)
    }

    /// One lane call, the way a caller reaches a catalog operation.
    fn lane_call(
        lane: &'static str,
        operation_id: &str,
        arguments: Value,
    ) -> CallToolRequestParams {
        let mut fields = Map::from_iter([("operation_id".to_string(), json!(operation_id))]);
        if arguments
            .as_object()
            .is_some_and(|object| !object.is_empty())
        {
            fields.insert("arguments".to_string(), arguments);
        }
        CallToolRequestParams::new(lane).with_arguments(fields)
    }

    #[test]
    fn every_published_operation_is_reachable_through_exactly_one_lane() {
        // The coverage guarantee, restated for a surface that no longer names
        // each operation: the catalog is exhaustive, so every operation it
        // publishes must route to a lane and appear in the index a caller
        // searches. Nothing may be stranded by the collapse.
        let index = catalog_index();
        let indexed: std::collections::HashSet<&str> = index
            .lines()
            .skip(1)
            .filter_map(|line| line.split('\t').next())
            .collect();

        let mut routed = 0usize;
        for operation in operation_catalog()
            .operations
            .iter()
            .filter(|operation| operation.exposed && !operation.deprecated.is_deprecated())
        {
            let lane = lanes::lane_of(operation);
            let arguments = lanes::LaneArguments {
                operation_id: operation.tool_name.clone(),
                arguments: Map::new(),
            };
            lanes::resolve(lane, &arguments).unwrap_or_else(|error| {
                panic!(
                    "{} does not route to {}: {error:?}",
                    operation.tool_name,
                    lane.tool_name()
                )
            });
            assert!(
                indexed.contains(operation.tool_name.as_str()),
                "{} is unindexed",
                operation.tool_name
            );
            routed += 1;
        }
        assert!(
            routed > 400,
            "the catalog should be exhaustive, routed {routed}"
        );

        let payload = GiteaMcp::list_tools_payload();
        let version = payload
            .tools
            .iter()
            .find(|tool| tool.name == SERVER_VERSION_TOOL)
            .expect("server.version");
        assert_eq!(
            version.input_schema.get("type"),
            Some(&Value::String("object".to_string()))
        );
        assert_eq!(
            version.input_schema.get("additionalProperties"),
            Some(&Value::Bool(false))
        );
    }

    #[test]
    fn curated_operation_names_and_search_guidance_survive_the_collapse() {
        // The disambiguated names and the agent guidance are properties of the
        // catalog, so they must still reach a caller through describe now that
        // no per-operation tool carries them.
        let described = discovery::describe(
            &discovery::DescribeArguments {
                name: "repository.get".to_string(),
                detail: None,
            },
            &GiteaMcp::list_tools_payload().tools,
        )
        .expect("repository.get");
        let guidance = described["guidance"].as_str().expect("agent guidance");
        for term in ["settings", "default branch", "permissions"] {
            assert!(guidance.contains(term), "guidance must include {term}");
        }

        let named = |name: &str| exposed_operation(name).is_some();
        assert!(named("repository.create_deploy_key"));
        assert!(!named("repository.create_key"));
        assert!(named("repository.update_pull_request_branch"));
        assert!(named("user.list_actions_runners"));
        assert!(!named("user.get_user_runners"));
    }

    #[test]
    fn every_registered_schema_compiles_as_draft_four() {
        let compiles = |name: &str, schema: &Map<String, Value>| {
            let schema = Value::Object(schema.clone());
            jsonschema::draft4::options()
                .build(&schema)
                .unwrap_or_else(|error| panic!("invalid schema for {name}: {error}"));
        };

        for tool in GiteaMcp::list_tools_payload().tools {
            for schema in
                std::iter::once(tool.input_schema.as_ref()).chain(tool.output_schema.as_deref())
            {
                compiles(&tool.name, schema);
            }
        }

        // The published listing no longer carries the generated operation
        // schemas, so they are compiled here from the catalog directly. They
        // are the schemas a lane validates against and describe hands out; a
        // malformed one would make its operation unreachable at runtime while
        // every published tool still compiled.
        for operation in &operation_catalog().operations {
            if operation.exposed {
                compiles(&operation.tool_name, &operation.input_schema);
            }
        }
    }

    #[tokio::test]
    async fn version_tool_rejects_unknown_arguments_before_upstream_call() {
        let client = Arc::new(
            GiteaClient::new(
                "https://gitea.example.test",
                "test-token",
                Duration::from_secs(1),
            )
            .expect("client"),
        );
        let request = CallToolRequestParams::new(SERVER_VERSION_TOOL)
            .with_arguments(Map::from_iter([("ignored".to_string(), json!(true))]));
        let error = test_mcp(client)
            .invoke_tool(request)
            .await
            .expect_err("unknown arguments must fail");
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn version_tool_returns_structured_upstream_version() {
        let (upstream_url, upstream) = loopback_json(
            "GET /api/v1/version HTTP/1.1\r\n",
            r#"{"version":"1.26.4"}"#,
        )
        .await;
        let client = Arc::new(
            GiteaClient::new(&upstream_url, "test-token", Duration::from_secs(1)).expect("client"),
        );

        let result = test_mcp(client)
            .invoke_tool(CallToolRequestParams::new(SERVER_VERSION_TOOL))
            .await
            .expect("valid version tool call");
        assert_eq!(
            result.structured_content,
            Some(json!({ "version": "1.26.4" }))
        );
        upstream.await.expect("upstream task");
    }

    #[test]
    fn deprecated_operations_are_unlisted_but_described_by_exact_name() {
        let deprecated: Vec<&OperationSpec> = operation_catalog()
            .operations
            .iter()
            .filter(|operation| operation.exposed && operation.deprecated.is_deprecated())
            .collect();
        assert!(!deprecated.is_empty(), "the pinned spec deprecates paths");
        let payload = GiteaMcp::list_tools_payload();
        let index = catalog_index();
        for operation in &deprecated {
            assert!(
                !payload
                    .tools
                    .iter()
                    .any(|tool| tool.name == operation.tool_name),
                "{} must not be published",
                operation.tool_name
            );
            assert!(
                !index.contains(&operation.tool_name),
                "{} must not appear in the index",
                operation.tool_name
            );
            // Exact-name describe still answers, because the operation stays
            // dispatchable and a caller holding its name deserves its contract.
            assert!(exposed_operation(&operation.tool_name).is_some());
        }
    }

    #[tokio::test]
    async fn deprecated_operations_stay_dispatchable() {
        let client = Arc::new(
            GiteaClient::new(
                "https://gitea.example.test",
                "test-token",
                Duration::from_secs(1),
            )
            .expect("client"),
        );
        // Reaching argument validation through the lane proves the operation
        // is still routed and executable; the upstream is never contacted
        // because the arguments are incomplete. Asserting the validation
        // message rather than the bare code matters here: the direct-name
        // refusal shares that code, so a weaker assertion would pass even if
        // the lane had stopped accepting the name.
        let error = test_mcp(client)
            .invoke_tool(lane_call(
                lanes::DESTROY_TOOL,
                "issue.delete_comment_deprecated",
                json!({}),
            ))
            .await
            .expect_err("missing path parameters must fail validation");
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(
            error.message.contains("generated Gitea schema"),
            "must fail schema validation, not routing: {}",
            error.message
        );
    }

    #[test]
    fn the_published_surface_is_the_lanes_and_the_hand_written_tools() {
        let published = GiteaMcp::list_tools_payload().tools;
        let names: Vec<&str> = published.iter().map(|tool| tool.name.as_ref()).collect();

        for expected in [
            "server.version",
            "access_token.create",
            "access_token.list",
            "access_token.revoke",
            "repository.bootstrap",
            "repository.secret.set_from_file",
            "catalog.search",
            "catalog.describe",
            "api.read",
            "api.mutate",
            "api.destroy",
            "api.admin",
        ] {
            assert!(names.contains(&expected), "{expected} is not published");
        }
        // Exactly those: no catalog operation is published as a tool of its
        // own, which is what keeps its risk class enforced by lane routing
        // rather than merely annotated.
        assert_eq!(published.len(), 12, "published surface: {names:?}");
        assert!(!names.contains(&"repository.get"));
    }

    #[test]
    fn repository_secret_tool_requests_gateway_file_upload() {
        let tool = GiteaMcp::list_tools_payload()
            .tools
            .into_iter()
            .find(|tool| tool.name == repository_secret::TOOL_NAME)
            .expect("repository secret tool");
        assert_eq!(
            tool.input_schema["properties"]["data_file"]["x-mcp-file"]["transferModes"],
            json!(["upload"])
        );
        let schema = Value::Object((*tool.input_schema).clone());
        let validator = jsonschema::draft4::new(&schema).expect("schema compiles");
        assert!(validator.is_valid(&json!({
            "owner": "alice",
            "repo": "demo",
            "name": "CI_SECRET",
            "data_file": "mcp-file://gateway/one-time-upload"
        })));
        assert_eq!(
            tool.annotations
                .and_then(|annotations| annotations.destructive_hint),
            Some(true)
        );
    }

    #[tokio::test]
    async fn invalid_repository_target_does_not_consume_staged_secret() {
        let client = Arc::new(
            GiteaClient::new(
                "https://gitea.example.test",
                "test-token",
                Duration::from_secs(1),
            )
            .expect("client"),
        );
        let files = files::FilePlane::new("https://gitea.example.test", Duration::from_mins(1))
            .expect("file plane");
        let value = b"fixture-value-never-production";
        let authorized = files
            .authorize_upload(files::AuthorizeUploadParams::default())
            .expect("upload authorization");
        let id = authorized.upload.url.rsplit('/').next().expect("upload id");
        let credential = &authorized.upload.headers[files::TRANSFER_CREDENTIAL_HEADER];
        files.receive(id, credential, value).expect("staged file");

        let error = test_mcp(client)
            .with_files(Some(Arc::clone(&files)))
            .invoke_tool(
                CallToolRequestParams::new(repository_secret::TOOL_NAME).with_arguments(
                    Map::from_iter([
                        ("owner".to_owned(), json!("..")),
                        ("repo".to_owned(), json!("demo")),
                        ("name".to_owned(), json!("CI_SECRET")),
                        ("data_file".to_owned(), json!(authorized.file.uri)),
                    ]),
                ),
            )
            .await
            .expect_err("invalid target");

        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        let secret = files
            .take_secret(&authorized.file.uri)
            .expect("validation leaves the file available");
        assert_eq!(secret.as_str(), "fixture-value-never-production");
    }

    #[tokio::test]
    async fn repository_secret_file_is_consumed_and_sent_once() {
        let (upstream_url, upstream) = loopback_json_with_request_body(
            "PUT /api/v1/repos/alice/demo/actions/secrets/CI_SECRET HTTP/1.1\r\n",
            r#"{"data":"fixture-value-never-production"}"#,
            r#"{"data":"fixture-value-never-production"}"#,
        )
        .await;
        let client = Arc::new(
            GiteaClient::new(&upstream_url, "test-token", Duration::from_secs(1)).expect("client"),
        );
        let files = files::FilePlane::new("https://gitea.example.test", Duration::from_mins(1))
            .expect("file plane");
        let authorized = files
            .authorize_upload(files::AuthorizeUploadParams::default())
            .expect("upload authorization");
        let id = authorized.upload.url.rsplit('/').next().expect("upload id");
        let credential = &authorized.upload.headers[files::TRANSFER_CREDENTIAL_HEADER];
        files
            .receive(id, credential, b"fixture-value-never-production")
            .expect("staged file");

        let result = test_mcp(client)
            .with_files(Some(Arc::clone(&files)))
            .invoke_tool(
                CallToolRequestParams::new(repository_secret::TOOL_NAME).with_arguments(
                    Map::from_iter([
                        ("owner".to_owned(), json!("alice")),
                        ("repo".to_owned(), json!("demo")),
                        ("name".to_owned(), json!("CI_SECRET")),
                        ("data_file".to_owned(), json!(authorized.file.uri)),
                    ]),
                ),
            )
            .await
            .expect("repository secret update");

        let structured = result.structured_content.expect("structured");
        assert_eq!(structured["operation_id"], json!("updateRepoSecret"));
        assert_eq!(structured["data"], Value::Null);
        assert!(
            !structured
                .to_string()
                .contains("fixture-value-never-production")
        );
        assert!(matches!(
            files.take_secret(&authorized.file.uri),
            Err(files::FileError::Unavailable)
        ));
        upstream.await.expect("upstream task");
    }

    #[tokio::test]
    async fn a_catalog_operation_name_is_refused_with_the_lane_that_runs_it() {
        // The migration answer for a caller holding a name from the retired
        // per-operation surface — and the assertion that direct dispatch stays
        // closed, which is the security half of the collapse. Driven through
        // invoke_tool, because restoring direct dispatch is exactly what this
        // must fail on.
        let client = Arc::new(
            GiteaClient::new(
                "https://gitea.example.test",
                "test-token",
                Duration::from_secs(1),
            )
            .expect("client"),
        );
        let error = test_mcp(client)
            .invoke_tool(
                CallToolRequestParams::new("repository.delete").with_arguments(Map::from_iter([
                    ("owner".to_string(), json!("alice")),
                    ("repo".to_string(), json!("demo")),
                ])),
            )
            .await
            .expect_err("a catalog operation is not a tool");
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(
            error.message.contains(lanes::DESTROY_TOOL),
            "the refusal must name the lane: {}",
            error.message
        );
        assert_eq!(lane_for("repository.delete"), Some(lanes::DESTROY_TOOL));
        assert_eq!(lane_for("access_token.create"), None);
    }

    #[test]
    fn the_orientation_describes_selection_then_execution() {
        assert!(SERVER_INSTRUCTIONS.contains("catalog.search"));
        assert!(SERVER_INSTRUCTIONS.contains("api.read"));
        assert!(SERVER_INSTRUCTIONS.contains("api.admin"));
        // The superseded flat orientation told callers to call generated tools
        // directly; nothing may say that now.
        assert!(!SERVER_INSTRUCTIONS.contains("Call the generated tools"));
    }

    #[tokio::test]
    async fn lane_dispatch_executes_with_identity_metadata() {
        let (upstream_url, upstream) = loopback_json(
            "GET /api/v1/repos/alice/demo HTTP/1.1\r\n",
            r#"{"id":1,"name":"demo"}"#,
        )
        .await;
        let client = Arc::new(
            GiteaClient::new(&upstream_url, "test-token", Duration::from_secs(1)).expect("client"),
        );

        let request =
            CallToolRequestParams::new(lanes::READ_TOOL).with_arguments(Map::from_iter([
                ("operation_id".to_string(), json!("repository.get")),
                (
                    "arguments".to_string(),
                    json!({"owner": "alice", "repo": "demo"}),
                ),
            ]));
        let result = test_mcp(client)
            .invoke_tool(request)
            .await
            .expect("lane call succeeds");
        let structured = result.structured_content.expect("structured envelope");
        assert_eq!(structured["operation_id"], json!("repoGet"));
        assert_eq!(structured["success"], json!(true));
        let meta = result.meta.expect("identity metadata");
        assert_eq!(meta.0["org.cacahuate/operationId"], json!("repoGet"));
        assert_eq!(meta.0["org.cacahuate/risk"], json!("read"));
        assert_eq!(meta.0["org.cacahuate/administrative"], json!(false));
        upstream.await.expect("upstream task");
    }

    #[tokio::test]
    async fn lane_routing_refuses_the_wrong_lane_by_name() {
        let client = Arc::new(
            GiteaClient::new(
                "https://gitea.example.test",
                "test-token",
                Duration::from_secs(1),
            )
            .expect("client"),
        );
        let mcp = test_mcp(client);

        let error = mcp
            .invoke_tool(CallToolRequestParams::new(lanes::READ_TOOL).with_arguments(
                Map::from_iter([("operation_id".to_string(), json!("repository.delete"))]),
            ))
            .await
            .expect_err("a destructive operation must not run on the read lane");
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(error.message.contains("api.destroy"));

        let error = mcp
            .invoke_tool(
                CallToolRequestParams::new(lanes::MUTATE_TOOL).with_arguments(Map::from_iter([(
                    "operation_id".to_string(),
                    json!("adminCreateUser"),
                )])),
            )
            .await
            .expect_err("an administrative operation must not run on the mutate lane");
        assert!(error.message.contains("api.admin"));
    }

    #[tokio::test]
    async fn lane_arguments_are_validated_before_the_upstream_call() {
        let client = Arc::new(
            GiteaClient::new(
                "https://gitea.example.test",
                "test-token",
                Duration::from_secs(1),
            )
            .expect("client"),
        );
        let error = test_mcp(client)
            .invoke_tool(CallToolRequestParams::new(lanes::READ_TOOL).with_arguments(
                Map::from_iter([("operation_id".to_string(), json!("repository.get"))]),
            ))
            .await
            .expect_err("missing path parameters must fail before any request");
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn lane_replaced_operation_points_at_its_hand_written_tool() {
        let client = Arc::new(
            GiteaClient::new(
                "https://gitea.example.test",
                "test-token",
                Duration::from_secs(1),
            )
            .expect("client"),
        );
        let error = test_mcp(client)
            .invoke_tool(CallToolRequestParams::new(lanes::READ_TOOL).with_arguments(
                Map::from_iter([("operation_id".to_string(), json!("getVersion"))]),
            ))
            .await
            .expect_err("a ledger-replaced operation is not catalog-executed");
        assert!(error.message.contains("server.version"));
    }

    #[tokio::test]
    async fn lane_suggestions_never_advertise_deprecated_operations() {
        let client = Arc::new(
            GiteaClient::new(
                "https://gitea.example.test",
                "test-token",
                Duration::from_secs(1),
            )
            .expect("client"),
        );
        let error = test_mcp(client)
            .invoke_tool(
                CallToolRequestParams::new(lanes::DESTROY_TOOL).with_arguments(Map::from_iter([(
                    "operation_id".to_string(),
                    json!("issue.delete_comment_deprecatd"),
                )])),
            )
            .await
            .expect_err("unknown identifier");
        assert!(
            !error.message.contains("deprecated"),
            "suggestions leak a deprecated name: {}",
            error.message
        );
    }

    #[tokio::test]
    async fn a_failed_lane_call_carries_the_resolved_identity_in_its_error() {
        // An unreachable upstream fails the executor after resolution, which
        // exercises the enrichment in the real dispatch path rather than the
        // helper alone.
        let client = Arc::new(
            GiteaClient::new(
                "https://gitea.invalid.example.test",
                "test-token",
                Duration::from_secs(1),
            )
            .expect("client"),
        );
        let error = test_mcp(client)
            .invoke_tool(CallToolRequestParams::new(lanes::READ_TOOL).with_arguments(
                Map::from_iter([
                    ("operation_id".to_string(), json!("repository.get")),
                    (
                        "arguments".to_string(),
                        json!({"owner": "alice", "repo": "demo"}),
                    ),
                ]),
            ))
            .await
            .expect_err("the upstream is unreachable");
        let data = error.data.expect("error data");
        assert_eq!(data["operation_id"], json!("repoGet"));
        assert_eq!(data["risk"], json!("read"));
        assert_eq!(data["administrative"], json!(false));
        assert!(
            data.get("outcome").is_some(),
            "outcome survives beside identity"
        );
    }

    #[test]
    fn result_meta_carries_sensitivity_and_identity_together() {
        let operation = exposed_operation("repository.get").expect("spec");
        let meta = result_meta(true, Some(operation)).expect("meta");
        assert_eq!(meta.0[SENSITIVE_RESULT_META], json!(true));
        assert_eq!(meta.0["org.cacahuate/operationId"], json!("repoGet"));
        assert!(result_meta(false, None).is_none());
    }

    #[tokio::test]
    async fn a_lane_name_as_operation_id_is_named_a_lane() {
        let client = Arc::new(
            GiteaClient::new(
                "https://gitea.example.test",
                "test-token",
                Duration::from_secs(1),
            )
            .expect("client"),
        );
        let error = test_mcp(client)
            .invoke_tool(CallToolRequestParams::new(lanes::READ_TOOL).with_arguments(
                Map::from_iter([("operation_id".to_string(), json!("api.read"))]),
            ))
            .await
            .expect_err("a lane is not an operation");
        assert!(error.message.contains("is a lane"));
        assert!(!error.message.contains("call it directly"));
    }

    #[tokio::test]
    async fn lane_points_a_hand_written_tool_name_home() {
        let client = Arc::new(
            GiteaClient::new(
                "https://gitea.example.test",
                "test-token",
                Duration::from_secs(1),
            )
            .expect("client"),
        );
        let error = test_mcp(client)
            .invoke_tool(
                CallToolRequestParams::new(lanes::DESTROY_TOOL).with_arguments(Map::from_iter([(
                    "operation_id".to_string(),
                    json!("repository.bootstrap"),
                )])),
            )
            .await
            .expect_err("a hand-written tool is not a catalog operation");
        assert!(error.message.contains("call it directly"));
    }

    #[tokio::test]
    async fn lane_unknown_operation_reports_nearest_names() {
        let client = Arc::new(
            GiteaClient::new(
                "https://gitea.example.test",
                "test-token",
                Duration::from_secs(1),
            )
            .expect("client"),
        );
        let error = test_mcp(client)
            .invoke_tool(CallToolRequestParams::new(lanes::READ_TOOL).with_arguments(
                Map::from_iter([("operation_id".to_string(), json!("repository.gte"))]),
            ))
            .await
            .expect_err("unknown identifier");
        assert!(error.message.contains("repository.get"));
    }

    #[test]
    fn lane_tools_publish_derived_worst_case_hints() {
        for tool in lanes::tools() {
            let annotations = tool.annotations.as_ref().expect("annotations");
            let meta = tool.meta.as_ref().expect("metadata channel");
            match tool.name.as_ref() {
                "api.read" => {
                    assert_eq!(annotations.read_only_hint, Some(true));
                    assert_eq!(annotations.destructive_hint, Some(false));
                }
                "api.admin" => {
                    assert_eq!(meta.0["org.cacahuate/administrative"], json!(true));
                    assert_eq!(annotations.destructive_hint, Some(true));
                }
                "api.destroy" => assert_eq!(annotations.destructive_hint, Some(true)),
                "api.mutate" => {
                    assert_eq!(annotations.read_only_hint, Some(false));
                    assert_eq!(annotations.destructive_hint, Some(false));
                }
                other => panic!("unexpected lane {other}"),
            }
            assert_eq!(
                tool.input_schema.get("additionalProperties"),
                Some(&Value::Bool(false))
            );
        }
    }

    #[tokio::test]
    async fn discovery_tools_dispatch_without_an_upstream() {
        let client = Arc::new(
            GiteaClient::new(
                "https://gitea.example.test",
                "test-token",
                Duration::from_secs(1),
            )
            .expect("client"),
        );
        let mcp = test_mcp(client);

        let search = mcp
            .invoke_tool(CallToolRequestParams::new("catalog.search").with_arguments(
                Map::from_iter([("query".to_string(), json!("repository.get"))]),
            ))
            .await
            .expect("search dispatches");
        let structured = search.structured_content.expect("structured search result");
        assert_eq!(structured["matches"][0]["tool"], json!("repository.get"));

        let described = mcp
            .invoke_tool(
                CallToolRequestParams::new("catalog.describe").with_arguments(Map::from_iter([(
                    "name".to_string(),
                    json!("repository.get"),
                )])),
            )
            .await
            .expect("describe dispatches");
        let structured = described
            .structured_content
            .expect("structured describe result");
        assert_eq!(structured["operation_id"], json!("repoGet"));
        assert!(structured["input_schema"].is_object());
    }

    #[tokio::test]
    async fn a_lane_validates_arguments_before_the_upstream_call() {
        let client = Arc::new(
            GiteaClient::new(
                "https://gitea.example.test",
                "test-token",
                Duration::from_secs(1),
            )
            .expect("client"),
        );

        let error = test_mcp(client)
            .invoke_tool(lane_call(lanes::READ_TOOL, "repository.get", json!({})))
            .await
            .expect_err("missing path parameters must fail");
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(
            error.message.contains("generated Gitea schema"),
            "must fail schema validation: {}",
            error.message
        );
    }

    #[tokio::test]
    async fn a_lane_rejects_unknown_nested_body_fields() {
        let client = Arc::new(
            GiteaClient::new(
                "https://gitea.example.test",
                "test-token",
                Duration::from_secs(1),
            )
            .expect("client"),
        );
        let error = test_mcp(client)
            .invoke_tool(lane_call(
                lanes::MUTATE_TOOL,
                "repository.create_branch",
                json!({
                    "owner": "alice",
                    "repo": "demo",
                    "body": {
                        "new_branch_name": "feature",
                        "unexpected": "must not reach Gitea"
                    }
                }),
            ))
            .await
            .expect_err("unknown nested fields must fail");
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn a_lane_dispatches_a_typed_path_and_structured_result() {
        let (upstream_url, upstream) = loopback_json(
            "GET /api/v1/repos/alice/demo HTTP/1.1\r\n",
            r#"{"id":1,"name":"demo"}"#,
        )
        .await;
        let client = Arc::new(
            GiteaClient::new(&upstream_url, "test-token", Duration::from_secs(1)).expect("client"),
        );
        let result = test_mcp(client)
            .invoke_tool(lane_call(
                lanes::READ_TOOL,
                "repository.get",
                json!({"owner": "alice", "repo": "demo"}),
            ))
            .await
            .expect("lane call");
        assert_eq!(
            result.structured_content,
            Some(json!({
                "operation_id": "repoGet",
                "status": 200,
                "success": true,
                "content_type": "application/json",
                "headers": {},
                "data": {"id": 1, "name": "demo"}
            }))
        );
        assert_eq!(result.is_error, Some(false));
        upstream.await.expect("upstream task");
    }

    #[tokio::test]
    async fn generated_sensitive_header_result_survives_mcp_dispatch() {
        let (upstream_url, upstream) = loopback_json_with_headers(
            "POST /api/v1/admin/actions/runners/registration-token HTTP/1.1\r\n",
            "",
            "token: runner-registration-secret\r\n",
        )
        .await;
        let client = Arc::new(
            GiteaClient::new(&upstream_url, "test-token", Duration::from_secs(1)).expect("client"),
        );

        let result = test_mcp(client)
            .invoke_tool(lane_call(
                lanes::ADMIN_TOOL,
                "admin.create_runner_registration_token",
                json!({}),
            ))
            .await
            .expect("runner registration token");

        assert_eq!(
            result.structured_content,
            Some(json!({
                "operation_id": "adminCreateRunnerRegistrationToken",
                "status": 200,
                "success": true,
                "content_type": "application/json",
                "headers": {"token": "runner-registration-secret"},
                "data": null
            }))
        );
        assert_eq!(
            result
                .meta
                .as_ref()
                .and_then(|meta| meta.0.get("org.cacahuate/sensitiveResult")),
            Some(&Value::Bool(true))
        );
        upstream.await.expect("upstream task");
    }

    #[tokio::test]
    async fn pat_only_sessions_refuse_token_work_before_any_upstream_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let client = Arc::new(
            GiteaClient::new(
                &format!("http://{}", listener.local_addr().expect("address")),
                "test-token",
                Duration::from_secs(1),
            )
            .expect("client"),
        );
        let mcp = GiteaMcp::new(client, None).new_session();
        for (name, arguments) in [
            (
                ACCESS_TOKEN_CREATE_TOOL,
                json!({"name":"agent","scopes":["read:user"]}),
            ),
            (ACCESS_TOKEN_LIST_TOOL, json!({})),
            (ACCESS_TOKEN_REVOKE_TOOL, json!({"id":7})),
            (
                bootstrap::TOOL_NAME,
                json!({"owner":"operator", "owner_kind":"current_user",
                "repository":{"name":"demo"}, "access_token":{"name":"agent","scopes":["read:user"]}}),
            ),
        ] {
            let error = mcp
                .invoke_tool(
                    CallToolRequestParams::new(name)
                        .with_arguments(arguments.as_object().expect("object").clone()),
                )
                .await
                .expect_err("token administration requires configuration");
            assert!(
                error.message.contains("GITEA_MCP_TOKEN_USERNAME"),
                "{error}"
            );
            assert!(
                error.message.contains("no upstream request was sent"),
                "{error}"
            );
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(30), listener.accept())
                .await
                .is_err()
        );
        let discovery = mcp
            .invoke_tool(
                CallToolRequestParams::new(discovery::SEARCH_TOOL).with_arguments(Map::new()),
            )
            .await
            .expect("discovery without token credentials");
        assert_ne!(discovery.is_error, Some(true));
    }

    #[tokio::test]
    async fn a_refused_token_create_reaches_the_caller_as_a_tool_error() {
        // Driven through `invoke_tool`, because the routing is what changed.
        // The three `upstream_failure` unit tests below cover the shape of the
        // value it returns; none of them notice if the dispatch stops calling
        // it, which is the defect this guards.
        let (upstream_url, upstream) = loopback_basic(
            "POST /api/v1/users/token-user/tokens HTTP/1.1\r\n",
            "422 Unprocessable Entity",
            r#"{"message":"access token name has already been used"}"#,
        )
        .await;
        let client = Arc::new(
            GiteaClient::new(
                "https://gitea.example.test",
                "test-token",
                Duration::from_secs(1),
            )
            .expect("client"),
        );
        let token_client = Arc::new(
            TokenLifecycleClient::new(
                &upstream_url,
                "token-user",
                "token-password",
                Duration::from_secs(1),
            )
            .expect("token client"),
        );
        let request =
            CallToolRequestParams::new(ACCESS_TOKEN_CREATE_TOOL).with_arguments(Map::from_iter([
                ("name".to_string(), json!("repo-agent")),
                ("scopes".to_string(), json!(["write:repository"])),
            ]));

        let mcp = GiteaMcp::new(client, token_client);
        let result = mcp
            .invoke_tool(request)
            .await
            .expect("a refusal is a tool result, not a protocol error");

        assert_eq!(result.is_error, Some(true));
        let rendered = format!("{:?}", result.content);
        assert!(
            rendered.contains("already been used"),
            "the reason reaches the caller: {rendered}"
        );
        assert!(
            result.structured_content.is_none(),
            "a refusal is not the token shape the output schema declares"
        );
        let meta = result.meta.expect("outcome metadata");
        assert_eq!(meta.0[OUTCOME_META]["status"], json!(422));
        upstream.await.expect("upstream task");
    }

    #[tokio::test]
    async fn access_token_create_returns_one_time_secret_with_sensitive_metadata() {
        let (upstream_url, upstream) = loopback_basic(
            "POST /api/v1/users/token-user/tokens HTTP/1.1\r\n",
            "201 Created",
            r#"{"id":9,"name":"repo-agent","scopes":["write:repository"],"sha1":"one-time-token","token_last_eight":"me-token","created_at":null}"#,
        )
        .await;
        let client = Arc::new(
            GiteaClient::new(
                "https://gitea.example.test",
                "test-token",
                Duration::from_secs(1),
            )
            .expect("client"),
        );
        let token_client = Arc::new(
            TokenLifecycleClient::new(
                &upstream_url,
                "token-user",
                "token-password",
                Duration::from_secs(1),
            )
            .expect("token client"),
        );
        let request =
            CallToolRequestParams::new(ACCESS_TOKEN_CREATE_TOOL).with_arguments(Map::from_iter([
                ("name".to_string(), json!("repo-agent")),
                ("scopes".to_string(), json!(["write:repository"])),
            ]));

        let mcp = GiteaMcp::new(client, token_client);
        let result = mcp.invoke_tool(request).await.expect("token creation");

        assert_eq!(
            result.structured_content,
            Some(json!({
                "id": 9,
                "name": "repo-agent",
                "scopes": ["write:repository"],
                "token": "one-time-token",
                "token_last_eight": "me-token",
                "created_at": null
            }))
        );
        assert_eq!(
            result
                .meta
                .as_ref()
                .and_then(|meta| meta.0.get("org.cacahuate/sensitiveResult")),
            Some(&Value::Bool(true))
        );
        upstream.await.expect("upstream task");

        let invalid =
            CallToolRequestParams::new(ACCESS_TOKEN_CREATE_TOOL).with_arguments(Map::from_iter([
                ("name".to_string(), json!("repo-agent")),
                ("scopes".to_string(), json!(["all", "read:user"])),
            ]));
        let error = mcp
            .invoke_tool(invalid)
            .await
            .expect_err("invalid scope combination");
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);

        for name in ["7", "0007", "+7", "-7"] {
            let numeric_name = CallToolRequestParams::new(ACCESS_TOKEN_CREATE_TOOL).with_arguments(
                Map::from_iter([
                    ("name".to_string(), json!(name)),
                    ("scopes".to_string(), json!(["read:user"])),
                ]),
            );
            let error = mcp
                .invoke_tool(numeric_name)
                .await
                .expect_err("decimal token name is ambiguous with a token ID");
            assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        }
    }

    #[tokio::test]
    async fn access_token_list_and_revoke_dispatch_to_basic_lane() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("listener address");
        let upstream = tokio::spawn(async move {
            for (request_line, status, body) in [
                (
                    "GET /api/v1/users/token-user/tokens?page=2&limit=25 HTTP/1.1\r\n",
                    "200 OK",
                    r#"[{"id":9,"name":"repo-agent","scopes":["write:repository"],"sha1":"must-not-return","token_last_eight":"me-token","created_at":null,"last_used_at":null}]"#,
                ),
                (
                    "DELETE /api/v1/users/token-user/tokens/repo-agent HTTP/1.1\r\n",
                    "204 No Content",
                    "",
                ),
            ] {
                let (mut socket, _) = listener.accept().await.expect("request");
                let mut request = vec![0_u8; 8_192];
                let length = socket.read(&mut request).await.expect("read request");
                let request = String::from_utf8_lossy(&request[..length]);
                assert!(request.starts_with(request_line));
                assert!(
                    request.to_ascii_lowercase().contains(
                        "authorization: basic dG9rZW4tdXNlcjp0b2tlbi1wYXNzd29yZA=="
                            .to_ascii_lowercase()
                            .as_str()
                    )
                );
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }
        });
        let client = Arc::new(
            GiteaClient::new(
                "https://gitea.example.test",
                "test-token",
                Duration::from_secs(1),
            )
            .expect("client"),
        );
        let token_client = Arc::new(
            TokenLifecycleClient::new(
                &format!("http://{address}"),
                "token-user",
                "token-password",
                Duration::from_secs(1),
            )
            .expect("token client"),
        );
        let mcp = GiteaMcp::new(client, token_client);

        let listed = mcp
            .invoke_tool(
                CallToolRequestParams::new(ACCESS_TOKEN_LIST_TOOL).with_arguments(Map::from_iter(
                    [
                        ("page".to_string(), json!(2)),
                        ("limit".to_string(), json!(25)),
                    ],
                )),
            )
            .await
            .expect("list");
        assert_eq!(
            listed.structured_content,
            Some(json!({
                "tokens": [{
                    "id": 9,
                    "name": "repo-agent",
                    "scopes": ["write:repository"],
                    "token_last_eight": "me-token",
                    "created_at": null,
                    "last_used_at": null
                }]
            }))
        );

        let revoked = mcp
            .invoke_tool(
                CallToolRequestParams::new(ACCESS_TOKEN_REVOKE_TOOL)
                    .with_arguments(Map::from_iter([("name".to_string(), json!("repo-agent"))])),
            )
            .await
            .expect("revoke");
        assert_eq!(revoked.structured_content, Some(json!({"revoked": true})));

        upstream.await.expect("upstream task");
    }

    #[tokio::test]
    async fn revoking_without_exactly_one_selector_is_refused() {
        // The published schema names `id` and `name` as optional and does not
        // state the choice between them, so a call carrying both or neither is
        // refused only here. A `null` counts as supplied: it would otherwise
        // deserialize to the same `None` an omitted field produces, and a
        // two-selector call would execute as a one-selector call. No upstream
        // is contacted: every case is settled before a request is built, which
        // is why this needs no listener.
        let client = Arc::new(
            GiteaClient::new(
                "https://gitea.example.test",
                "test-token",
                Duration::from_secs(1),
            )
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
        let mcp = GiteaMcp::new(client, token_client);

        for arguments in [
            Map::from_iter([
                ("id".to_string(), json!(9)),
                ("name".to_string(), json!("repo-agent")),
            ]),
            Map::from_iter([]),
            Map::from_iter([
                ("id".to_string(), Value::Null),
                ("name".to_string(), json!("repo-agent")),
            ]),
            Map::from_iter([
                ("id".to_string(), json!(9)),
                ("name".to_string(), Value::Null),
            ]),
            Map::from_iter([("id".to_string(), Value::Null)]),
        ] {
            let error = mcp
                .invoke_tool(
                    CallToolRequestParams::new(ACCESS_TOKEN_REVOKE_TOOL).with_arguments(arguments),
                )
                .await
                .expect_err("exactly one selector is required");
            assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        }
    }

    async fn loopback_status(
        status: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("listener address");
        let upstream = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request");
            let mut request = vec![0_u8; 8_192];
            let _length = socket.read(&mut request).await.expect("read request");
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });
        (format!("http://{address}"), upstream)
    }

    async fn loopback_json(
        expected_request_line: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        loopback_json_with_headers(expected_request_line, body, "").await
    }

    async fn loopback_json_with_request_body(
        expected_request_line: &'static str,
        expected_request_body: &'static str,
        response_body: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("listener address");
        let upstream = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request");
            let mut request = Vec::with_capacity(8_192);
            let body_start = loop {
                let mut chunk = [0_u8; 4_096];
                let length = socket.read(&mut chunk).await.expect("read request");
                assert_ne!(length, 0, "request ended before the body arrived");
                request.extend_from_slice(&chunk[..length]);
                assert!(request.len() <= 65_536, "request exceeded test bound");
                if let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let body_start = header_end + 4;
                    if request.len() >= body_start + expected_request_body.len() {
                        break body_start;
                    }
                }
            };
            let request_text = String::from_utf8_lossy(&request);
            assert!(request_text.starts_with(expected_request_line));
            assert!(
                request_text
                    .to_ascii_lowercase()
                    .contains("authorization: token test-token\r\n")
            );
            let request_body =
                std::str::from_utf8(&request[body_start..body_start + expected_request_body.len()])
                    .expect("request body");
            assert_eq!(request_body, expected_request_body);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });
        (format!("http://{address}"), upstream)
    }

    async fn loopback_json_with_headers(
        expected_request_line: &'static str,
        body: &'static str,
        extra_headers: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("listener address");
        let upstream = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request");
            let mut request = vec![0_u8; 65536];
            let length = socket.read(&mut request).await.expect("read request");
            let request = String::from_utf8_lossy(&request[..length]);
            assert!(request.starts_with(expected_request_line));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: token test-token\r\n")
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n{extra_headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });
        (format!("http://{address}"), upstream)
    }

    async fn loopback_basic(
        expected_request_line: &'static str,
        status: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("listener address");
        let upstream = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request");
            let mut request = vec![0_u8; 8_192];
            let length = socket.read(&mut request).await.expect("read request");
            let request = String::from_utf8_lossy(&request[..length]);
            assert!(request.starts_with(expected_request_line));
            assert!(
                request.to_ascii_lowercase().contains(
                    "authorization: basic dG9rZW4tdXNlcjp0b2tlbi1wYXNzd29yZA=="
                        .to_ascii_lowercase()
                        .as_str()
                )
            );
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });
        (format!("http://{address}"), upstream)
    }

    #[test]
    fn a_payload_under_the_ceiling_is_returned_inline() {
        let mcp = bounded_mcp(small_policy(1024));
        let result = mcp
            .bound_response(&response(json!("short log"), "text/plain"), None, None)
            .expect("bounded");
        let structured = result.structured_content.expect("structured");
        assert_eq!(structured["data"], json!("short log"));
        assert!(mcp.store.list().is_empty(), "nothing should be stored");
    }

    #[test]
    fn a_payload_over_the_ceiling_becomes_a_resource_instead_of_being_inlined() {
        // The whole point of the tier: the caller gets an actionable handle and
        // a preview, and the megabyte never reaches its context.
        let mcp = bounded_mcp(small_policy(1024));
        let log = "x".repeat(65536);
        let result = mcp
            .bound_response(&response(json!(log.clone()), "text/plain"), None, None)
            .expect("bounded");
        let structured = result.structured_content.expect("structured");

        assert_eq!(structured["payload"]["inlined"], json!(false));
        assert_eq!(structured["payload"]["bytes"], json!(65536));
        assert!(
            structured.get("data").is_none(),
            "payload must not be inlined"
        );

        let uri = structured["payload"]["resource_uri"]
            .as_str()
            .expect("resource uri");
        assert_eq!(
            mcp.store.read(uri).map(|stored| stored.body),
            Some(log.into_bytes()),
            "the stored payload must be the whole original"
        );
    }

    #[test]
    fn an_oversized_response_carries_a_resource_link_a_client_can_follow() {
        let mcp = bounded_mcp(small_policy(1024));
        let result = mcp
            .bound_response(
                &response(json!("x".repeat(65536)), "text/plain"),
                None,
                None,
            )
            .expect("bounded");
        let uri = result
            .structured_content
            .as_ref()
            .and_then(|value| value["payload"]["resource_uri"].as_str())
            .expect("resource uri")
            .to_string();
        let linked = result.content.iter().any(|block| {
            matches!(block, rmcp::model::ContentBlock::ResourceLink(link) if link.uri == uri)
        });
        assert!(linked, "structured uri and resource link must agree");
    }

    #[test]
    fn a_text_payload_is_previewed_so_the_caller_can_triage_without_a_second_call() {
        // The ceiling has to leave room for an excerpt. A reply repeats its
        // document as an escaped text block, so a 1 KiB ceiling is consumed by
        // the envelope and handle alone and the excerpt is correctly shed.
        let ceiling = 8192;
        let mcp = bounded_mcp(small_policy(ceiling));
        let log = format!("ERROR failed to compile\n{}", "x".repeat(65536));
        let result = mcp
            .bound_response(&response(json!(log), "text/plain"), None, None)
            .expect("bounded");
        let structured = result.structured_content.clone().expect("structured");
        let preview = structured["payload"]["preview"].as_str().expect("preview");
        assert!(preview.starts_with("ERROR failed to compile"));
        assert!(preview.len() <= RESOURCE_PREVIEW_BYTES);
        assert!(
            measured_len(&result) <= ceiling,
            "the excerpt must not push the reply past the ceiling"
        );
    }

    #[test]
    fn a_stored_json_payload_round_trips_as_json_not_as_a_quoted_string() {
        // Reading the resource must yield the document itself; a caller that
        // asked for JSON and received an escaped copy of it has to unwrap it.
        let mcp = bounded_mcp(small_policy(64));
        let document = json!({"entries": vec!["value"; 4000]});
        let result = mcp
            .bound_response(&response(document.clone(), "application/json"), None, None)
            .expect("bounded");
        let uri = result
            .structured_content
            .and_then(|value| {
                value["payload"]["resource_uri"]
                    .as_str()
                    .map(str::to_string)
            })
            .expect("resource uri");
        let stored = mcp.store.read(&uri).expect("stored");
        assert_eq!(stored.content_type, "application/json");
        assert_eq!(
            serde_json::from_slice::<Value>(&stored.body).expect("json"),
            document
        );
    }

    #[test]
    fn a_failure_after_the_call_ran_tells_the_caller_not_to_repeat_it() {
        // What the model actually reads. A displaced or undecodable response on
        // a mutation must not arrive looking like a request that never left,
        // because the standing instruction for that is to send it again.
        let error = map_operation_error(&ApiError::ResponseNotDelivered {
            status: 200,
            source: Box::new(ApiError::ResponseTooLarge),
        });
        assert!(
            error.message.contains("HTTP 200") && error.message.contains("do not reissue"),
            "message must name the status and discourage a repeat: {}",
            error.message
        );
        let data = error.data.expect("machine-readable outcome");
        assert_eq!(data["outcome"], json!("completed"));
        assert_eq!(data["status"], json!(200));
        assert_eq!(
            data["safe_to_retry"],
            json!(false),
            "a completed call is never safe to repeat"
        );
    }

    #[test]
    fn a_refused_token_call_is_a_tool_error_not_a_protocol_error() {
        // Gitea saying no is not this server malfunctioning. A protocol error
        // tells a client the request could not be processed at all, which hides
        // a status that was delivered perfectly well and sends a caller looking
        // for a fault in the wrong place.
        let result = upstream_failure(&ApiError::Upstream {
            status: 422,
            message: "access token name has already been used".to_string(),
        })
        .expect("a refusal is a tool result");
        assert_eq!(result.is_error, Some(true));
        let rendered = format!("{:?}", result.content);
        assert!(
            rendered.contains("already been used"),
            "the reason reaches the caller: {rendered}"
        );
        assert!(
            result.structured_content.is_none(),
            "a refusal is not the token shape the output schema declares"
        );
        let meta = result.meta.expect("outcome metadata");
        assert_eq!(meta.0[OUTCOME_META]["outcome"], json!("completed"));
        assert_eq!(meta.0[OUTCOME_META]["status"], json!(422));
        assert_eq!(meta.0[OUTCOME_META]["safe_to_retry"], json!(false));
    }

    #[test]
    fn a_refused_version_call_is_also_a_tool_error() {
        // server.version is the third hand-written tool and had the same
        // protocol-error framing. Reachability matters more than the tool does:
        // a client that special-cases protocol failures would treat a Gitea
        // refusal here differently from the identical refusal on any other tool.
        let result = upstream_failure(&ApiError::Upstream {
            status: 503,
            message: "server is in maintenance mode".to_string(),
        })
        .expect("a refusal is a tool result");
        assert_eq!(result.is_error, Some(true));
        let rendered = format!("{:?}", result.content);
        assert!(
            rendered.contains("maintenance mode"),
            "the reason reaches the caller: {rendered}"
        );
    }

    #[test]
    fn invalid_token_input_stays_a_protocol_error() {
        // The counterpart: a call that was never valid to make is a fault in
        // the request, not an answer from Gitea, and belongs at the protocol
        // level where a client will not mistake it for an upstream refusal.
        let error = upstream_failure(&ApiError::InvalidAccessTokenInput(
            "token identifier must be positive",
        ))
        .expect_err("bad input is a protocol error");
        assert!(error.message.contains("must be positive"));
    }

    #[test]
    fn a_failure_that_could_not_have_run_stays_safe_to_retry() {
        // The other half of the contract. If every failure warned against
        // retrying, the warning would carry no information and a caller would
        // learn to ignore it — including on the mutation that matters.
        let error = map_operation_error(&ApiError::InvalidArguments("owner".to_string()));
        let data = error.data.expect("machine-readable outcome");
        assert_eq!(data["outcome"], json!("not_sent"));
        assert_eq!(data["safe_to_retry"], json!(true));
        assert!(
            error.message.contains("never sent"),
            "message must say nothing happened: {}",
            error.message
        );
    }

    #[test]
    fn an_unanswered_request_is_reported_as_an_unknown_outcome() {
        // Distinct from both: something went out, nothing came back. Claiming
        // either "it ran" or "it did not" would be an invention.
        let error = map_operation_error(&ApiError::ResponseTooLarge);
        let data = error.data.expect("machine-readable outcome");
        assert_eq!(data["outcome"], json!("sent_outcome_unknown"));
        assert_eq!(data["safe_to_retry"], json!(false));
        assert!(
            error.message.contains("unknown"),
            "message must not resolve what is unresolved: {}",
            error.message
        );
    }

    #[test]
    fn an_error_response_is_never_displaced_into_a_resource() {
        // Error bodies are already tightly bounded upstream and carry the
        // recovery detail; hiding one behind a URI would cost a round trip to
        // learn why the call failed.
        let mcp = bounded_mcp(small_policy(8));
        let mut failure = response(json!("boom".repeat(100)), "text/plain");
        failure.success = false;
        failure.status = 500;
        let result = mcp
            .operation_result(failure, false, None, None)
            .expect("result");
        assert!(result.structured_content.expect("structured")["data"].is_string());
        assert!(mcp.store.list().is_empty());
    }

    #[tokio::test]
    async fn a_large_token_list_displaces_through_the_dispatch() {
        // Driven through `invoke_tool`, because the gap was the dispatch
        // returning its own result rather than the fitting step being wrong. A
        // test that calls `bound_value` directly stays green when the branch
        // stops calling it — which is precisely how this defect survived.
        let tokens: Vec<Value> = (0..100)
            .map(|id| {
                json!({
                    "id": id + 1,
                    "name": format!("{id}-{}", "n".repeat(200)),
                    "scopes": ["read:user"],
                    "token_last_eight": "eightchr",
                    "created_at": null,
                    "last_used_at": null
                })
            })
            .collect();
        let body: &'static str = Box::leak(
            serde_json::to_string(&tokens)
                .expect("token list")
                .into_boxed_str(),
        );
        let (upstream_url, upstream) = loopback_basic(
            "GET /api/v1/users/token-user/tokens?page=1&limit=100 HTTP/1.1\r\n",
            "200 OK",
            body,
        )
        .await;
        let client = Arc::new(
            GiteaClient::new(
                "https://gitea.example.test",
                "test-token",
                Duration::from_secs(1),
            )
            .expect("client"),
        );
        let token_client = Arc::new(
            TokenLifecycleClient::new(
                &upstream_url,
                "token-user",
                "token-password",
                Duration::from_secs(1),
            )
            .expect("token client"),
        );
        let mcp = GiteaMcp::with_large_content_policy(client, token_client, small_policy(8_192));
        let request =
            CallToolRequestParams::new(ACCESS_TOKEN_LIST_TOOL).with_arguments(Map::from_iter([
                ("page".to_string(), json!(1)),
                ("limit".to_string(), json!(100)),
            ]));

        let result = mcp.invoke_tool(request).await.expect("token list");
        let structured = result.structured_content.clone().expect("structured");
        assert!(
            structured.get("payload").is_some(),
            "a list this size must not be inlined: {structured}"
        );
        assert_eq!(structured["payload"]["retained"], json!(true));
        assert!(
            measured_len(&result) <= 8_192,
            "the reply itself must be under the ceiling"
        );
        upstream.await.expect("upstream task");
    }

    #[tokio::test]
    async fn a_large_bootstrap_report_displaces_through_the_dispatch() {
        // The other hand-written branch whose result grows with its input. The
        // all-five test below calls `bound_value` directly, so reverting this
        // branch's fitting call leaves it green — the same gap that let the
        // defect exist. Driven through `invoke_tool` for that reason.
        //
        // One upstream call: the repository lookup is refused with a long
        // reason, which the step detail carries, so a single failed step is
        // enough to cross a 4 KiB ceiling once the reply repeats its document.
        let refusal: &'static str =
            Box::leak(format!(r#"{{"message":"{}"}}"#, "d".repeat(2_000)).into_boxed_str());
        let (upstream_url, upstream) = loopback_status("422 Unprocessable Entity", refusal).await;
        let client = Arc::new(
            GiteaClient::new(&upstream_url, "test-token", Duration::from_secs(1)).expect("client"),
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
        let mcp = GiteaMcp::with_large_content_policy(client, token_client, small_policy(4_096))
            .with_execution_limit(1);
        let request =
            CallToolRequestParams::new(bootstrap::TOOL_NAME).with_arguments(Map::from_iter([
                ("owner".to_string(), json!("acme")),
                ("owner_kind".to_string(), json!("organization")),
                ("repository".to_string(), json!({"name": "widget"})),
            ]));

        let result = mcp.invoke_tool(request).await.expect("bootstrap runs");
        let structured = result.structured_content.clone().expect("structured");
        assert!(
            structured.get("payload").is_some(),
            "a report this size must not be inlined: {structured}"
        );
        assert!(
            measured_len(&result) <= 4_096,
            "the reply itself must be under the ceiling"
        );
        assert_eq!(
            result.is_error,
            Some(true),
            "a failed bootstrap is still a failure once displaced"
        );
        upstream.await.expect("upstream task");
    }

    #[test]
    fn a_nested_token_shape_does_not_advertise_displacement() {
        // Displacement replaces a whole result, never a field inside one. The
        // created-token shape is used in both places, and sharing one definition
        // made the bootstrap report advertise `{tool, payload}` for its
        // `access_token` field — a shape nothing can produce there.
        let bootstrap_tool = GiteaMcp::list_tools_payload()
            .tools
            .into_iter()
            .find(|tool| tool.name == bootstrap::TOOL_NAME)
            .expect("bootstrap tool");
        let schema = Value::Object((*bootstrap_tool.output_schema.expect("output schema")).clone());
        let nested = &schema["properties"]["access_token"];
        let rendered = nested.to_string();
        assert!(
            !rendered.contains("\"payload\""),
            "the nested token shape must not offer a displaced alternative: {rendered}"
        );

        // The tool's own output still does, because that reply can be displaced.
        let create_tool = GiteaMcp::list_tools_payload()
            .tools
            .into_iter()
            .find(|tool| tool.name == ACCESS_TOKEN_CREATE_TOOL)
            .expect("create tool");
        let create_schema =
            Value::Object((*create_tool.output_schema.expect("output schema")).clone());
        assert!(
            create_schema["properties"].get("payload").is_some(),
            "the tool's own reply can be displaced: {create_schema}"
        );
    }

    #[test]
    fn every_hand_written_tool_displaces_and_still_satisfies_its_schema() {
        // The gap this closes: these five built their own results and returned
        // them whatever the size, so the ceiling was a property of which branch
        // ran rather than of the reply. Checked for the original five rather than the two
        // that can realistically reach the ceiling, because "this one cannot get
        // big" is the assumption that stops being true quietly.
        let mcp = bounded_mcp(small_policy(1024));
        let tools = GiteaMcp::list_tools_payload().tools;

        for (name, value) in [
            (SERVER_VERSION_TOOL, json!({"version": "x".repeat(65_536)})),
            (
                ACCESS_TOKEN_CREATE_TOOL,
                json!({"id": 1, "name": "x".repeat(65_536), "scopes": [], "token": "t",
                       "token_last_eight": "eight", "created_at": null}),
            ),
            (
                ACCESS_TOKEN_LIST_TOOL,
                json!({"tokens": [{"id": 1, "name": "x".repeat(65_536), "scopes": [],
                                   "token_last_eight": "eight", "created_at": null,
                                   "last_used_at": null}]}),
            ),
            (ACCESS_TOKEN_REVOKE_TOOL, json!({"revoked": true})),
            (
                bootstrap::TOOL_NAME,
                json!({"complete": false, "repository_created": false,
                                          "steps": [], "compensations": [],
                                          "detail": "x".repeat(65_536)}),
            ),
        ] {
            let tool = tools
                .iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("{name} is registered"));
            let schema =
                Value::Object((**tool.output_schema.as_ref().expect("output schema")).clone());
            let validator = jsonschema::draft4::new(&schema).expect("schema compiles");

            let structured = mcp
                .bound_value(name, &value, false)
                .unwrap_or_else(|_| panic!("{name} bounds"))
                .structured_content
                .unwrap_or_else(|| panic!("{name} is structured"));

            // `revoked: true` is far under the ceiling and must stay inline;
            // everything else here is over it and must have been displaced.
            let displaced = structured.get("payload").is_some();
            assert_eq!(
                displaced,
                name != ACCESS_TOKEN_REVOKE_TOOL,
                "{name} displacement: {structured}"
            );
            assert!(
                validator.is_valid(&structured),
                "{name} reply must satisfy its own schema: {structured}"
            );
        }
    }

    #[test]
    fn a_displaced_token_result_keeps_its_sensitivity_when_read_back() {
        // The marker travels with the payload, not with the reply. Asserting it
        // on the CallToolResult alone proves only half the path: storing the
        // payload as ordinary would leave that assertion green while the later
        // resources/read hands the credential over unclassified.
        let mcp = bounded_mcp(small_policy(1024));
        let result = mcp
            .bound_value(
                ACCESS_TOKEN_CREATE_TOOL,
                &json!({"id": 1, "name": "x".repeat(65_536), "scopes": [], "token": "secret",
                        "token_last_eight": "eight", "created_at": null}),
                true,
            )
            .expect("bounded");
        assert_eq!(
            result.meta.as_ref().expect("sensitivity metadata").0[SENSITIVE_RESULT_META],
            json!(true)
        );

        let uri =
            result.structured_content.as_ref().expect("structured")["payload"]["resource_uri"]
                .as_str()
                .expect("a retained payload has a URI")
                .to_string();
        let read = mcp.read_resource_uri(&uri).expect("the payload reads back");
        let meta = resource_meta(&read.contents[0]);
        assert_eq!(
            meta.expect("the stored payload is classified")
                .get(SENSITIVE_RESULT_META),
            Some(&Value::Bool(true)),
            "the store was told what it was holding"
        );
    }

    #[test]
    fn a_displaced_result_satisfies_the_advertised_output_schema() {
        // The schema is what a validating client checks the reply against, so a
        // displaced payload that violates it would be rejected — losing exactly
        // the resource handle this feature exists to deliver.
        let mcp = bounded_mcp(small_policy(1024));
        let displaced = mcp
            .bound_response(
                &response(json!("x".repeat(65536)), "text/plain"),
                None,
                None,
            )
            .expect("bounded")
            .structured_content
            .expect("structured");
        let inlined = mcp
            .bound_response(&response(json!("short"), "text/plain"), None, None)
            .expect("bounded")
            .structured_content
            .expect("structured");

        // The lane advertises the contract every catalog result answers to.
        let tool = GiteaMcp::list_tools_payload()
            .tools
            .into_iter()
            .find(|tool| tool.name == lanes::READ_TOOL)
            .expect("api.read");
        let schema = Value::Object((*tool.output_schema.expect("output schema")).clone());
        let validator = jsonschema::draft4::new(&schema).expect("schema compiles");

        assert!(
            validator.is_valid(&displaced),
            "displaced result must validate"
        );
        assert!(validator.is_valid(&inlined), "inlined result must validate");
    }

    #[test]
    fn a_binary_payload_is_stored_as_bytes_not_as_its_base64_envelope() {
        // gitea-api hands binary back base64-encoded inside JSON, a third larger
        // than the original. Storing that expansion would push an artifact the
        // transport accepted past the object cap purely because of encoding.
        let mcp = bounded_mcp(small_policy(64));
        let raw = vec![0_u8; 65536];
        let envelope = json!({"encoding": "base64", "content": BASE64_STANDARD.encode(&raw)});
        let result = mcp
            .bound_response(&response(envelope, "application/zip"), None, None)
            .expect("bounded");
        let structured = result.structured_content.expect("structured");

        assert_eq!(structured["payload"]["bytes"], json!(65536));
        let uri = structured["payload"]["resource_uri"].as_str().expect("uri");
        let stored = mcp.store.read(uri).expect("stored");
        assert_eq!(stored.body, raw, "the original bytes, not the envelope");
        assert_eq!(stored.content_type, "application/zip");
    }

    #[test]
    fn one_session_cannot_read_another_sessions_payloads() {
        // Being authenticated to this server does not entitle a conversation to
        // another conversation's job logs.
        let first = bounded_mcp(small_policy(64));
        let second = first.new_session();

        let uri = first
            .bound_response(
                &response(json!("x".repeat(65536)), "text/plain"),
                None,
                None,
            )
            .expect("bounded")
            .structured_content
            .and_then(|value| {
                value["payload"]["resource_uri"]
                    .as_str()
                    .map(str::to_string)
            })
            .expect("uri");

        assert!(
            first.store.read(&uri).is_some(),
            "its own session can read it"
        );
        assert!(second.store.read(&uri).is_none(), "another session cannot");
        assert!(second.store.list().is_empty());
    }

    #[test]
    fn a_clone_within_one_session_still_shares_its_payloads() {
        // The transport clones the handler per request; the session boundary is
        // new_session, not Clone.
        let mcp = bounded_mcp(small_policy(64));
        let uri = mcp
            .bound_response(
                &response(json!("x".repeat(65536)), "text/plain"),
                None,
                None,
            )
            .expect("bounded")
            .structured_content
            .and_then(|value| {
                value["payload"]["resource_uri"]
                    .as_str()
                    .map(str::to_string)
            })
            .expect("uri");
        assert!(mcp.clone().store.read(&uri).is_some());
    }

    #[test]
    fn a_sensitive_payload_keeps_its_classification_when_read_back() {
        // The reply that displaced it is not the reply a gateway inspects.
        let mcp = bounded_mcp(small_policy(64));
        let uri = mcp
            .bound_response(
                &response(json!("t".repeat(65536)), "text/plain"),
                result_meta(true, None).as_ref(),
                None,
            )
            .expect("bounded")
            .structured_content
            .and_then(|value| {
                value["payload"]["resource_uri"]
                    .as_str()
                    .map(str::to_string)
            })
            .expect("uri");
        let stored = mcp.store.read(&uri).expect("stored");
        assert!(stored.sensitive, "classification travels with the payload");
    }

    fn resource_meta(contents: &rmcp::model::ResourceContents) -> Option<Meta> {
        match contents {
            rmcp::model::ResourceContents::TextResourceContents { meta, .. }
            | rmcp::model::ResourceContents::BlobResourceContents { meta, .. } => meta.clone(),
            _ => None,
        }
    }

    fn stored(sensitive: bool, body: Vec<u8>) -> resources::StoredResource {
        resources::StoredResource {
            uri: "gitea-response:/x/0".to_string(),
            operation_id: "x".to_string(),
            content_type: "text/plain".to_string(),
            sensitive,
            body,
        }
    }

    #[test]
    fn reading_a_sensitive_payload_carries_the_classification_meta() {
        let contents = resource_contents(stored(true, b"secret".to_vec()));
        let meta = resource_meta(&contents);
        assert_eq!(
            meta.expect("meta").get(SENSITIVE_RESULT_META),
            Some(&Value::Bool(true))
        );
    }

    #[test]
    fn reading_an_ordinary_payload_carries_no_classification_meta() {
        let contents = resource_contents(stored(false, b"plain".to_vec()));
        let meta = resource_meta(&contents);
        assert!(meta.is_none());
    }

    #[test]
    fn non_utf8_payloads_are_read_back_as_blobs() {
        let contents = resource_contents(stored(false, vec![0xff, 0xfe, 0x00]));
        assert!(matches!(
            contents,
            rmcp::model::ResourceContents::BlobResourceContents { .. }
        ));
    }

    #[tokio::test]
    async fn a_sensitive_operation_marks_the_payload_it_displaces() {
        // Covers the wiring from the generated classification through dispatch
        // into the store. Asserting on bound_response alone would pass even if
        // the call site stopped passing the operation's classification, which
        // is precisely the value a gateway depends on.
        let body: &'static str =
            Box::leak(format!(r#"{{"token":"{}"}}"#, "s".repeat(80_000)).into_boxed_str());
        let (upstream_url, upstream) = loopback_json(
            "POST /api/v1/admin/actions/runners/registration-token HTTP/1.1\r\n",
            body,
        )
        .await;
        let client = Arc::new(
            GiteaClient::new(&upstream_url, "test-token", Duration::from_secs(5)).expect("client"),
        );
        let mcp = test_mcp(client);

        let result = mcp
            .invoke_tool(lane_call(
                lanes::ADMIN_TOOL,
                "admin.create_runner_registration_token",
                json!({}),
            ))
            .await
            .expect("registration token");

        let uri = result
            .structured_content
            .and_then(|value| {
                value["payload"]["resource_uri"]
                    .as_str()
                    .map(str::to_string)
            })
            .expect("the oversized payload was displaced");
        let stored = mcp.store.read(&uri).expect("stored");
        assert!(
            stored.sensitive,
            "a credential-bearing payload must stay classified once displaced"
        );
        upstream.await.expect("upstream task");
    }

    #[test]
    fn a_json_body_that_merely_contains_encoding_and_content_is_kept_whole() {
        // Gitea's own ContentsResponse carries encoding and content alongside
        // the name, SHA, size, and links a caller needs. Treating it as the
        // transport's base64 envelope would store the file bytes and silently
        // discard everything else.
        let mcp = bounded_mcp(small_policy(64));
        let contents = json!({
            "name": "README.md",
            "sha": "abc123",
            "size": 4096,
            "encoding": "base64",
            "content": BASE64_STANDARD.encode(vec![b'r'; 4096]),
            "download_url": "https://gitea.example.test/raw/README.md",
        });
        let uri = mcp
            .bound_response(&response(contents.clone(), "application/json"), None, None)
            .expect("bounded")
            .structured_content
            .and_then(|value| {
                value["payload"]["resource_uri"]
                    .as_str()
                    .map(str::to_string)
            })
            .expect("uri");
        let stored = mcp.store.read(&uri).expect("stored");

        assert_eq!(stored.content_type, "application/json");
        assert_eq!(
            serde_json::from_slice::<Value>(&stored.body).expect("json"),
            contents,
            "the whole response, not just the decoded file"
        );
    }

    #[test]
    fn binary_media_types_are_read_back_as_blobs_even_when_the_bytes_decode() {
        // A zip or image can be accidentally valid UTF-8. Choosing text on that
        // basis would hand the caller mojibake instead of the file.
        let contents = resource_contents(resources::StoredResource {
            uri: "gitea-response:/x/0".to_string(),
            operation_id: "x".to_string(),
            content_type: "application/zip".to_string(),
            sensitive: false,
            body: b"PK-but-valid-utf8".to_vec(),
        });
        assert!(matches!(
            contents,
            rmcp::model::ResourceContents::BlobResourceContents { .. }
        ));
    }

    #[test]
    fn textual_media_types_are_read_back_as_text() {
        for media_type in [
            "text/plain",
            "application/json",
            "text/x-diff; charset=utf-8",
        ] {
            let contents = resource_contents(resources::StoredResource {
                uri: "gitea-response:/x/0".to_string(),
                operation_id: "x".to_string(),
                content_type: media_type.to_string(),
                sensitive: false,
                body: b"readable".to_vec(),
            });
            assert!(
                matches!(
                    contents,
                    rmcp::model::ResourceContents::TextResourceContents { .. }
                ),
                "{media_type} should read back as text"
            );
        }
    }

    #[test]
    fn sessions_share_one_retained_byte_budget() {
        // Per-session stores keep one conversation from reading another's
        // payloads, but they all occupy one process. Without a shared account
        // every session would be entitled to the full aggregate cap and total
        // retention would grow without bound in the number of sessions.
        let policy = LargeContentPolicy {
            context_ceiling_bytes: 64,
            limits: resources::ResourceLimits {
                max_object_bytes: 131_072,
                max_total_bytes: 100_000,
                time_to_live: Duration::from_mins(1),
            },
        };
        let first = bounded_mcp(policy);
        let second = first.new_session();

        let retained = first
            .bound_response(
                &response(json!("a".repeat(65536)), "text/plain"),
                None,
                None,
            )
            .expect("bounded")
            .structured_content
            .expect("structured");
        assert_eq!(retained["payload"]["retained"], json!(true));

        // The second session's call still succeeds — the upstream read happened
        // — but the shared account is full, so its body is not kept.
        let refused = second
            .bound_response(
                &response(json!("b".repeat(65536)), "text/plain"),
                None,
                None,
            )
            .expect("a completed call must not become an error")
            .structured_content
            .expect("structured");

        assert_eq!(refused["success"], json!(true));
        assert_eq!(
            refused["payload"]["retained"],
            json!(false),
            "the budget is shared, so the second session cannot retain its payload"
        );
        let detail = refused["payload"]["detail"].as_str().expect("detail");
        assert!(
            detail.contains("shared resource storage"),
            "the detail should name the shared budget: {detail}"
        );
    }

    #[test]
    fn releasing_a_payload_returns_its_bytes_to_the_shared_budget() {
        // The payload must be small enough to be retained and large enough to be
        // displaced, or nothing is stored and the assertion below passes against
        // an untouched budget — which is what an earlier revision of this test
        // did after its payload size was raised without revisiting its limits.
        let body = 8192;
        let total = body + 2048;
        let policy = LargeContentPolicy {
            context_ceiling_bytes: 64,
            limits: resources::ResourceLimits {
                max_object_bytes: body * 2,
                max_total_bytes: total,
                time_to_live: Duration::from_mins(1),
            },
        };
        let mcp = bounded_mcp(policy);
        for round in 0..5 {
            let structured = mcp
                .bound_response(&response(json!("a".repeat(body)), "text/plain"), None, None)
                .expect("bounded")
                .structured_content
                .expect("structured");
            assert_eq!(
                structured["payload"]["retained"],
                json!(true),
                "round {round} must actually store a payload for release to be exercised"
            );
            assert!(
                mcp.budget.used_bytes() > 0,
                "a retained payload holds budget"
            );
        }
        assert!(
            mcp.budget.used_bytes() <= total,
            "five rounds recycle the same space rather than accumulating"
        );
    }

    #[test]
    fn a_completed_call_stays_successful_when_its_payload_cannot_be_retained() {
        // The upstream side effect has already happened. Turning a completed
        // mutation into an error that says "retry" invites a second submission
        // of something that already succeeded, which the repository's mutation
        // policy forbids.
        let policy = LargeContentPolicy {
            context_ceiling_bytes: 16,
            limits: resources::ResourceLimits {
                max_object_bytes: 32,
                max_total_bytes: 32,
                time_to_live: Duration::from_mins(1),
            },
        };
        let mcp = bounded_mcp(policy);
        let result = mcp
            .bound_response(
                &response(json!("x".repeat(65536)), "text/plain"),
                None,
                None,
            )
            .expect("a completed call must not become an error");
        let structured = result.structured_content.expect("structured");

        assert_eq!(structured["success"], json!(true));
        assert_eq!(structured["status"], json!(200));
        assert_eq!(structured["payload"]["retained"], json!(false));
        assert!(
            structured["payload"]["detail"].is_string(),
            "the reply should say why the body was not kept"
        );
        assert!(
            structured["payload"].get("resource_uri").is_none(),
            "there is no resource to follow"
        );
        assert!(
            !result
                .content
                .iter()
                .any(|block| matches!(block, rmcp::model::ContentBlock::ResourceLink(_))),
            "no link when nothing was retained"
        );
    }

    #[test]
    fn an_unretained_result_still_satisfies_the_output_schema() {
        let policy = LargeContentPolicy {
            context_ceiling_bytes: 16,
            limits: resources::ResourceLimits {
                max_object_bytes: 32,
                max_total_bytes: 32,
                time_to_live: Duration::from_mins(1),
            },
        };
        let structured = bounded_mcp(policy)
            .bound_response(
                &response(json!("x".repeat(65536)), "text/plain"),
                None,
                None,
            )
            .expect("bounded")
            .structured_content
            .expect("structured");
        // The lane advertises the contract every catalog result answers to.
        let tool = GiteaMcp::list_tools_payload()
            .tools
            .into_iter()
            .find(|tool| tool.name == lanes::READ_TOOL)
            .expect("api.read");
        let schema = Value::Object((*tool.output_schema.expect("output schema")).clone());
        let validator = jsonschema::draft4::new(&schema).expect("schema compiles");
        assert!(validator.is_valid(&structured));
    }

    #[test]
    fn the_ceiling_bounds_the_reply_not_the_decoded_payload() {
        // Base64 inflates binary by a third inside JSON. Measuring the decoded
        // bytes would inline a payload that sits at the ceiling as roughly a
        // third more than the ceiling allows.
        let ceiling = 4096;
        let mcp = bounded_mcp(small_policy(ceiling));
        // Decodes to just under the ceiling, serializes to well over it.
        let raw = vec![0_u8; 3600];
        let envelope = json!({"encoding": "base64", "content": BASE64_STANDARD.encode(&raw)});
        let structured = mcp
            .bound_response(&response(envelope, "application/zip"), None, None)
            .expect("bounded")
            .structured_content
            .expect("structured");

        assert_eq!(
            structured["payload"]["inlined"],
            json!(false),
            "its inline form exceeds the ceiling, so it must be displaced"
        );
        assert!(structured.get("data").is_none());
    }

    #[test]
    fn a_json_blob_response_shaped_like_the_envelope_is_kept_whole() {
        // GitBlobResponse declares encoding and content among seven fields, and
        // Gitea omits empty ones, so a blob can serialise to exactly the two
        // keys the transport wrapper uses. The media type is what separates
        // them: the wrapper is only ever synthesised for a non-JSON body.
        let mcp = bounded_mcp(small_policy(64));
        let blob = json!({
            "encoding": "base64",
            "content": BASE64_STANDARD.encode(vec![b'z'; 65536]),
        });
        let uri = mcp
            .bound_response(&response(blob.clone(), "application/json"), None, None)
            .expect("bounded")
            .structured_content
            .and_then(|value| {
                value["payload"]["resource_uri"]
                    .as_str()
                    .map(str::to_string)
            })
            .expect("uri");
        let stored = mcp.store.read(&uri).expect("stored");

        assert_eq!(stored.content_type, "application/json");
        assert_eq!(
            serde_json::from_slice::<Value>(&stored.body).expect("json"),
            blob,
            "a JSON response keeps its own shape"
        );
    }

    #[test]
    fn a_binary_body_of_the_same_shape_is_still_decoded() {
        let mcp = bounded_mcp(small_policy(64));
        let raw = vec![7_u8; 65536];
        let envelope = json!({"encoding": "base64", "content": BASE64_STANDARD.encode(&raw)});
        let uri = mcp
            .bound_response(&response(envelope, "application/octet-stream"), None, None)
            .expect("bounded")
            .structured_content
            .and_then(|value| {
                value["payload"]["resource_uri"]
                    .as_str()
                    .map(str::to_string)
            })
            .expect("uri");
        assert_eq!(mcp.store.read(&uri).expect("stored").body, raw);
    }

    #[test]
    fn no_recovery_message_directs_a_second_upstream_call() {
        // Every generated operation can displace a payload, destructive ones
        // included, and the call that produced a handle already completed.
        let policy = LargeContentPolicy {
            context_ceiling_bytes: 16,
            limits: resources::ResourceLimits {
                max_object_bytes: 32,
                max_total_bytes: 32,
                time_to_live: Duration::from_mins(1),
            },
        };
        let detail = bounded_mcp(policy)
            .bound_response(
                &response(json!("x".repeat(65536)), "text/plain"),
                None,
                None,
            )
            .expect("bounded")
            .structured_content
            .expect("structured")["payload"]["detail"]
            .as_str()
            .expect("detail")
            .to_string();
        assert_no_retry_directive(&detail, "object-too-large detail");

        // The other retention failure, reached only when a sibling session has
        // taken the shared budget.
        let shared = LargeContentPolicy {
            context_ceiling_bytes: 16,
            limits: resources::ResourceLimits {
                max_object_bytes: 131_072,
                max_total_bytes: 100_000,
                time_to_live: Duration::from_mins(1),
            },
        };
        let first = bounded_mcp(shared);
        let second = first.new_session();
        first
            .bound_response(
                &response(json!("a".repeat(65536)), "text/plain"),
                None,
                None,
            )
            .expect("bounded");
        let exhausted = second
            .bound_response(
                &response(json!("b".repeat(65536)), "text/plain"),
                None,
                None,
            )
            .expect("bounded")
            .structured_content
            .expect("structured")["payload"]["detail"]
            .as_str()
            .expect("detail")
            .to_string();
        assert_no_retry_directive(&exhausted, "budget-exhausted detail");

        // And the message a caller sees when the handle has already gone.
        assert_no_retry_directive(
            &missing_resource_message("gitea-response:/x/0"),
            "missing-resource message",
        );
    }

    fn assert_no_retry_directive(message: &str, what: &str) {
        let lowered = message.to_ascii_lowercase();
        for directive in ["retry", "re-run", "rerun", "run it again", "try again"] {
            assert!(
                !lowered.contains(directive),
                "{what} must not direct another call: {message}"
            );
        }
    }

    #[test]
    fn the_ceiling_measures_the_whole_reply_including_its_envelope() {
        // The envelope carries operation id, status, and declared headers. A
        // payload admitted on its own size can push the reply past the ceiling
        // once those are added.
        let mcp = bounded_mcp(small_policy(512));
        let ceiling = mcp.policy.context_ceiling_bytes;
        let mut wide = response(json!("x".repeat(ceiling / 2)), "text/plain");
        for index in 0..120 {
            wide.headers.insert(
                format!("x-declared-header-{index}"),
                Value::String("padding-value-for-the-envelope".to_string()),
            );
        }
        let structured = mcp
            .bound_response(&wide, None, None)
            .expect("bounded")
            .structured_content
            .expect("structured");
        assert_eq!(
            structured["payload"]["inlined"],
            json!(false),
            "the envelope pushes this reply above the ceiling"
        );
    }

    #[test]
    fn a_displaced_reply_is_itself_within_the_ceiling() {
        // The reply that replaces a payload carries every declared header and
        // an excerpt. Checking only the inline candidate would let the
        // replacement exceed the very limit that caused the displacement.
        let mcp = bounded_mcp(small_policy(512));
        let ceiling = mcp.policy.context_ceiling_bytes;
        let mut wide = response(json!("E".repeat(262_144)), "text/plain");
        for index in 0..120 {
            wide.headers.insert(
                format!("x-declared-header-{index}"),
                Value::String("padding-value-for-the-envelope".to_string()),
            );
        }
        let structured = mcp
            .bound_response(&wide, None, None)
            .expect("bounded")
            .structured_content
            .expect("structured");

        let serialized = serde_json::to_vec(&structured).expect("serialize").len();
        assert!(
            serialized <= ceiling,
            "displaced reply is {serialized} bytes, above the {ceiling}-byte ceiling"
        );
        assert_eq!(structured["payload"]["retained"], json!(true));
        assert!(
            structured["payload"]["resource_uri"].is_string(),
            "the handle survives shedding"
        );
    }

    #[test]
    fn shedding_headers_is_recorded_rather_than_silent() {
        let mcp = bounded_mcp(small_policy(512));
        let ceiling = mcp.policy.context_ceiling_bytes;
        let mut wide = response(json!("E".repeat(262_144)), "text/plain");
        for index in 0..120 {
            wide.headers.insert(
                format!("x-declared-header-{index}"),
                Value::String("padding-value-for-the-envelope".to_string()),
            );
        }
        let result = mcp.bound_response(&wide, None, None).expect("bounded");
        let structured = result.structured_content.clone().expect("structured");
        assert_eq!(structured["payload"]["headers_omitted"], json!(true));
        assert_eq!(structured["headers"], json!({}));
        assert!(
            measured_len(&result) <= ceiling,
            "shedding must bring the reply under the ceiling, not merely record a loss"
        );
    }

    #[test]
    fn a_narrow_displaced_reply_keeps_its_headers_and_preview() {
        // Shedding is a response to pressure, not the normal path.
        let mcp = bounded_mcp(small_policy(4096));
        let mut narrow = response(json!("D".repeat(262_144)), "text/plain");
        narrow
            .headers
            .insert("x-one".to_string(), Value::String("kept".to_string()));
        let structured = mcp
            .bound_response(&narrow, None, None)
            .expect("bounded")
            .structured_content
            .expect("structured");

        assert_eq!(structured["headers"]["x-one"], json!("kept"));
        assert!(structured["payload"].get("headers_omitted").is_none());
        assert!(
            structured["payload"]["preview"]
                .as_str()
                .is_some_and(|p| !p.is_empty())
        );
    }

    #[test]
    fn a_headerless_json_response_shaped_like_the_envelope_is_kept_whole() {
        // execute_operation decodes by the response header when present and
        // falls back to the operation's declared produces. Classifying here
        // without that fallback would read a headerless JSON body as binary.
        let mcp = bounded_mcp(small_policy(64));
        let blob = json!({
            "encoding": "base64",
            "content": BASE64_STANDARD.encode(vec![b'q'; 65536]),
        });
        let mut headerless = response(blob.clone(), "text/plain");
        headerless.content_type = None;

        let uri = mcp
            .bound_response(&headerless, None, Some("application/json"))
            .expect("bounded")
            .structured_content
            .and_then(|value| {
                value["payload"]["resource_uri"]
                    .as_str()
                    .map(str::to_string)
            })
            .expect("uri");
        let stored = mcp.store.read(&uri).expect("stored");
        assert_eq!(
            serde_json::from_slice::<Value>(&stored.body).expect("json"),
            blob,
            "a declared-JSON response keeps its own shape without a header"
        );
    }

    #[test]
    fn a_headerless_binary_response_is_still_decoded() {
        let mcp = bounded_mcp(small_policy(64));
        let raw = vec![3_u8; 65536];
        let envelope = json!({"encoding": "base64", "content": BASE64_STANDARD.encode(&raw)});
        let mut headerless = response(envelope, "text/plain");
        headerless.content_type = None;
        let uri = mcp
            .bound_response(&headerless, None, Some("application/octet-stream"))
            .expect("bounded")
            .structured_content
            .and_then(|value| {
                value["payload"]["resource_uri"]
                    .as_str()
                    .map(str::to_string)
            })
            .expect("uri");
        assert_eq!(mcp.store.read(&uri).expect("stored").body, raw);
    }

    #[test]
    fn every_displaced_reply_is_measured_as_the_transport_sends_it() {
        // The assembled result repeats its document as an escaped text block and
        // carries the resource link, so measuring structured_content alone
        // understates it by more than half.
        for requested in [512_usize, 2048, 8192, 65536] {
            let mcp = bounded_mcp(small_policy(requested));
            let ceiling = mcp.policy.context_ceiling_bytes;
            assert!(ceiling >= MINIMUM_CONTEXT_CEILING_BYTES);
            let mut wide = response(json!("\"quoted\"\n\t".repeat(4096)), "text/plain");
            for index in 0..20 {
                wide.headers.insert(
                    format!("x-declared-header-{index}"),
                    Value::String("padding-value-for-the-envelope".to_string()),
                );
            }
            let result = mcp.bound_response(&wide, None, None).expect("bounded");
            let serialized = measured_len(&result);
            assert!(
                serialized <= ceiling,
                "reply is {serialized} bytes against a {ceiling}-byte ceiling"
            );
        }
    }

    #[test]
    fn the_measured_reply_is_the_reply_the_caller_receives() {
        // is_error and the sensitivity metadata were once attached after the
        // fit, which put them outside every size decision. Since the excerpt
        // search consumes exactly the space left, anything added afterwards
        // pushed the reply back over the ceiling.
        let mcp = bounded_mcp(small_policy(4096));
        let mut wide = response(json!("S".repeat(262_144)), "text/plain");
        for index in 0..40 {
            wide.headers.insert(
                format!("x-declared-header-{index}"),
                Value::String("padding-value-for-the-envelope".to_string()),
            );
        }
        let ceiling = mcp.policy.context_ceiling_bytes;
        let result = mcp
            .bound_response(&wide, result_meta(true, None).as_ref(), None)
            .expect("bounded");

        assert_eq!(result.is_error, Some(false), "set inside the fit");
        assert_eq!(
            result
                .meta
                .as_ref()
                .and_then(|meta| meta.0.get(SENSITIVE_RESULT_META)),
            Some(&Value::Bool(true)),
            "sensitivity is inside the fit too"
        );
        assert!(
            measured_len(&result) <= ceiling,
            "reply is {} bytes against a {ceiling}-byte ceiling",
            measured_len(&result)
        );
    }

    #[test]
    fn a_pathological_content_type_cannot_push_the_reply_over() {
        // gitea-api copies the response Content-Type without a length bound, so
        // it is shed last rather than allowed to defeat the ceiling.
        let mcp = bounded_mcp(small_policy(4096));
        let ceiling = mcp.policy.context_ceiling_bytes;
        let absurd = format!("text/plain; charset={}", "x".repeat(8192));
        let result = mcp
            .bound_response(&response(json!("P".repeat(262_144)), &absurd), None, None)
            .expect("bounded");

        assert!(
            measured_len(&result) <= ceiling,
            "reply is {} bytes against a {ceiling}-byte ceiling",
            measured_len(&result)
        );
        let structured = result.structured_content.clone().expect("structured");
        assert!(
            structured["payload"]["resource_uri"].is_string(),
            "the handle survives even that"
        );
    }

    #[test]
    fn a_multibyte_preview_does_not_hang_the_fit() {
        // Snapping a byte midpoint back onto a character boundary can land below
        // the low bound inside a multibyte scalar, leaving the bound unchanged.
        // The upstream call has already succeeded at this point, so a hang here
        // costs a worker and loses the handle.
        for text in ["日本語のログ", "emoji 🚀 log", "mixed ascii and ünïcödé"] {
            let mcp = bounded_mcp(small_policy(4096));
            let payload = text.repeat(20_000);
            let result = mcp
                .bound_response(&response(json!(payload), "text/plain"), None, None)
                .expect("bounded");
            let structured = result.structured_content.clone().expect("structured");
            assert!(structured["payload"]["resource_uri"].is_string());
            assert!(measured_len(&result) <= mcp.policy.context_ceiling_bytes);
            if let Some(preview) = structured["payload"]["preview"].as_str() {
                assert!(
                    payload.starts_with(preview),
                    "preview must be a real prefix"
                );
            }
        }
    }

    #[test]
    fn resource_listings_are_paged_and_every_entry_is_reachable() {
        let mcp = bounded_mcp(small_policy(4096));
        let total = RESOURCE_PAGE_SIZE + 20;
        for index in 0..total {
            mcp.store
                .insert(&format!("op{index}"), "text/plain", false, vec![b'x'; 8])
                .expect("stored");
        }

        let (first, cursor) = mcp.store.page(None, RESOURCE_PAGE_SIZE);
        assert_eq!(first.len(), RESOURCE_PAGE_SIZE, "a page is bounded");
        let cursor = cursor.expect("more remains");

        let (second, done) = mcp.store.page(Some(&cursor), RESOURCE_PAGE_SIZE);
        assert_eq!(second.len(), 20);
        assert!(done.is_none(), "the walk terminates");

        let seen: std::collections::HashSet<_> = first
            .iter()
            .chain(second.iter())
            .map(|stored| stored.uri.clone())
            .collect();
        assert_eq!(seen.len(), total, "every entry is reachable exactly once");
        assert!(
            first.iter().all(|stored| stored.body.is_empty()),
            "a listing carries no bodies"
        );
    }

    #[test]
    fn a_json_string_payload_is_stored_as_json_not_as_bare_characters() {
        // Stripping the quoting is right for a log or a diff and wrong for a
        // JSON body that happens to decode to a string: the resource would be
        // labelled application/json while holding something that is not JSON.
        let mcp = bounded_mcp(small_policy(4096));
        let document = json!("a quoted \"string\" body with \\ escapes");
        let long = json!(format!(
            "{}{}",
            document.as_str().unwrap(),
            "x".repeat(262_144)
        ));
        let uri = mcp
            .bound_response(&response(long.clone(), "application/json"), None, None)
            .expect("bounded")
            .structured_content
            .and_then(|value| {
                value["payload"]["resource_uri"]
                    .as_str()
                    .map(str::to_string)
            })
            .expect("uri");
        let stored = mcp.store.read(&uri).expect("stored");

        assert_eq!(stored.content_type, "application/json");
        assert_eq!(
            serde_json::from_slice::<Value>(&stored.body).expect("valid json"),
            long,
            "a resource labelled JSON must parse as JSON"
        );
    }

    #[test]
    fn a_text_string_payload_is_still_stored_unquoted() {
        let mcp = bounded_mcp(small_policy(4096));
        let log = "ERROR\n".to_string() + &"x".repeat(262_144);
        let uri = mcp
            .bound_response(&response(json!(log.clone()), "text/plain"), None, None)
            .expect("bounded")
            .structured_content
            .and_then(|value| {
                value["payload"]["resource_uri"]
                    .as_str()
                    .map(str::to_string)
            })
            .expect("uri");
        let stored = mcp.store.read(&uri).expect("stored");
        assert_eq!(
            String::from_utf8(stored.body).expect("utf8"),
            log,
            "a log is the log, not a quoted copy of one"
        );
    }

    #[test]
    fn a_headerless_payload_keeps_the_media_type_used_to_decode_it() {
        // The declared type already decided how the body was decoded; labelling
        // the resource text/plain would discard it at the last step.
        let mcp = bounded_mcp(small_policy(4096));
        let mut headerless = response(
            json!("<html>".to_string() + &"x".repeat(262_144)),
            "text/plain",
        );
        headerless.content_type = None;
        let uri = mcp
            .bound_response(&headerless, None, Some("text/html"))
            .expect("bounded")
            .structured_content
            .and_then(|value| {
                value["payload"]["resource_uri"]
                    .as_str()
                    .map(str::to_string)
            })
            .expect("uri");
        assert_eq!(
            mcp.store.read(&uri).expect("stored").content_type,
            "text/html"
        );
    }

    #[test]
    fn the_index_names_every_exposed_tool_once() {
        let index = catalog_index();
        let names: Vec<&str> = index
            .lines()
            .skip(1)
            .filter_map(|line| line.split('\t').next())
            .collect();
        // Every name a caller can execute: the published tools that are not
        // lanes, plus the catalog operations the lanes run. Comparing catalog
        // to catalog could not notice a hand-written tool with no line, and
        // comparing to `tools/list` alone would now miss the operations.
        let mut registered: Vec<String> = GiteaMcp::list_tools_payload()
            .tools
            .into_iter()
            .filter(|tool| lanes::Lane::from_tool_name(&tool.name).is_none())
            .map(|tool| tool.name.to_string())
            .collect();
        registered.extend(
            operation_catalog()
                .operations
                .iter()
                .filter(|operation| operation.exposed && !operation.deprecated.is_deprecated())
                .map(|operation| operation.tool_name.clone()),
        );

        assert_eq!(
            names.len(),
            registered.len(),
            "one line per executable name"
        );
        assert_eq!(
            names.iter().collect::<std::collections::HashSet<_>>().len(),
            registered.len(),
            "and no duplicates"
        );
        for name in &registered {
            assert!(
                names.contains(&name.as_str()),
                "{name} is callable but absent from the index"
            );
        }
        for hand_written in [
            SERVER_VERSION_TOOL,
            ACCESS_TOKEN_CREATE_TOOL,
            ACCESS_TOKEN_LIST_TOOL,
            ACCESS_TOKEN_REVOKE_TOOL,
            bootstrap::TOOL_NAME,
            repository_secret::TOOL_NAME,
        ] {
            assert!(names.contains(&hand_written), "{hand_written} missing");
        }
    }

    #[test]
    fn every_index_line_carries_the_fields_a_caller_selects_on() {
        // The index exists so a tool can be chosen without loading schemas, so
        // a line missing its risk or its summary defeats the purpose.
        let index = catalog_index();
        for line in index.lines().skip(1) {
            let fields: Vec<&str> = line.split('\t').collect();
            assert_eq!(fields.len(), 5, "malformed line: {line}");
            assert!(!fields[0].is_empty(), "tool name: {line}");
            assert!(
                matches!(fields[1], "read" | "mutation" | "destructive"),
                "risk: {line} — an unrecognised tool yields the placeholder \"unknown\""
            );
            assert!(!fields[2].is_empty(), "domain: {line}");
            assert!(!fields[4].is_empty(), "summary: {line}");
        }
    }

    #[test]
    fn index_risk_and_required_match_the_catalog_for_every_operation_row() {
        // The other half of the index's contract. Operation rows no longer
        // have a published tool to be compared against, so they are held to
        // the spec the lanes validate and execute against — the only other
        // reader of the same fields.
        let index = catalog_index();
        let mut rows = std::collections::HashMap::new();
        for line in index.lines().skip(1) {
            let fields: Vec<&str> = line.split('\t').collect();
            rows.insert(
                fields[0].to_string(),
                (fields[1].to_string(), fields[3].to_string()),
            );
        }

        for operation in operation_catalog()
            .operations
            .iter()
            .filter(|operation| operation.exposed && !operation.deprecated.is_deprecated())
        {
            let (risk, required) = rows
                .get(&operation.tool_name)
                .unwrap_or_else(|| panic!("{} unindexed", operation.tool_name));
            let expected_risk = match operation.risk {
                OperationRisk::Read => "read",
                OperationRisk::Mutation => "mutation",
                OperationRisk::Destructive => "destructive",
            };
            assert_eq!(
                risk, expected_risk,
                "index misreports the risk of {}",
                operation.tool_name
            );
            for mandatory in operation
                .input_schema
                .get("required")
                .and_then(Value::as_array)
                .map(|names| names.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                .unwrap_or_default()
            {
                assert!(
                    required.contains(mandatory),
                    "index omits {mandatory}, required by {}",
                    operation.tool_name
                );
            }
        }
    }

    #[test]
    fn index_summaries_match_the_published_tool_summaries() {
        // Two views of one catalog. If they drift, the index sends a caller to a
        // tool whose description says something else.
        let index = catalog_index();
        let mut indexed = std::collections::HashMap::new();
        for line in index.lines().skip(1) {
            let fields: Vec<&str> = line.split('\t').collect();
            indexed.insert(fields[0].to_string(), fields[4].to_string());
        }
        for operation in operation_catalog()
            .operations
            .iter()
            .filter(|op| op.exposed && !op.deprecated.is_deprecated())
        {
            assert_eq!(
                indexed.get(&operation.tool_name),
                Some(&operation.summary),
                "index and catalog disagree for {}",
                operation.tool_name
            );
        }
    }

    #[test]
    fn the_instructions_stay_free_of_counts_that_move_with_the_catalog() {
        // They are part of every prompt prefix. A number that changes when the
        // pinned specification is regenerated would invalidate that cache for
        // no benefit to the reader.
        for moving in ["467", "471", "239", "165", "63", "25", "32"] {
            assert!(
                !SERVER_INSTRUCTIONS.contains(moving),
                "instructions cite {moving}, which moves with the catalog"
            );
        }
        assert!(SERVER_INSTRUCTIONS.contains(CATALOG_INDEX_URI));
        assert!(SERVER_INSTRUCTIONS.contains("catalog.search"));
        assert!(SERVER_INSTRUCTIONS.contains("catalog.describe"));
        assert!(SERVER_INSTRUCTIONS.contains("gitea-response:"));
        assert!(SERVER_INSTRUCTIONS.contains(SENSITIVE_RESULT_META));
    }

    #[test]
    fn index_risk_and_required_match_the_published_tool_for_every_entry() {
        // Compared against the registry a caller actually receives, not against
        // the generated catalog. A hand-maintained table disagreed with the
        // tools it described — bootstrap indexed as a mutation while its
        // annotations say destructive — and a catalog-to-catalog comparison
        // could not see it.
        let index = catalog_index();
        let mut rows = std::collections::HashMap::new();
        for line in index.lines().skip(1) {
            let fields: Vec<&str> = line.split('\t').collect();
            rows.insert(
                fields[0].to_string(),
                (fields[1].to_string(), fields[3].to_string()),
            );
        }

        for tool in GiteaMcp::list_tools_payload()
            .tools
            .into_iter()
            .filter(|tool| lanes::Lane::from_tool_name(&tool.name).is_none())
        {
            let name = tool.name.to_string();
            let annotations = tool.annotations.as_ref();
            let expected_risk = if annotations.and_then(|a| a.destructive_hint) == Some(true) {
                "destructive"
            } else if annotations.and_then(|a| a.read_only_hint) == Some(true) {
                "read"
            } else {
                "mutation"
            };
            let (risk, required) = rows
                .get(&name)
                .unwrap_or_else(|| panic!("{name} unindexed"));
            assert_eq!(risk, expected_risk, "index misreports the risk of {name}");

            // Asserted as a property rather than by recomputing the column the
            // way the index builds it, which would pass against any rule they
            // happened to share. The contract is that an argument the tool
            // cannot be called without is named, whether the schema states that
            // unconditionally or as a choice between branches.
            let mut mandatory: Vec<&str> = tool
                .input_schema
                .get("required")
                .and_then(Value::as_array)
                .map(|names| names.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if let Some(branches) = tool.input_schema.get("oneOf").and_then(Value::as_array) {
                for branch in branches {
                    mandatory.extend(
                        branch
                            .get("required")
                            .and_then(Value::as_array)
                            .map(|names| names.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                            .unwrap_or_default(),
                    );
                }
            }
            for argument in &mandatory {
                assert!(
                    required.split([',', '|']).any(|column| column == *argument),
                    "index omits the required argument {argument} of {name}: {required:?}"
                );
            }
            if mandatory.is_empty() {
                assert!(
                    required.is_empty(),
                    "index invents required arguments for {name}: {required:?}"
                );
            }
        }
    }

    #[test]
    fn the_destructive_bootstrap_workflow_is_indexed_as_destructive() {
        // Named separately because the instructions tell a caller to select on
        // the index, and this is the registered tool with the largest gap
        // between what it is called and what it does.
        let index = catalog_index();
        let line = index
            .lines()
            .find(|line| line.starts_with(bootstrap::TOOL_NAME))
            .expect("bootstrap is indexed");
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(fields[1], "destructive");
        assert!(
            fields[3].contains("owner") && fields[3].contains("repository"),
            "required arguments: {line}"
        );
    }

    #[test]
    fn reading_the_index_uri_serves_the_index() {
        // Covers the helper only. Whether `resources/read` still reaches it is
        // measured over a real session in `tests/protocol_surface.rs`: a test
        // that calls the helper directly stays green when the entry point stops
        // delegating to it.
        let mcp = bounded_mcp(small_policy(4096));
        let result = mcp
            .read_resource_uri(CATALOG_INDEX_URI)
            .expect("the advertised URI serves");
        match &result.contents[0] {
            rmcp::model::ResourceContents::TextResourceContents {
                text, mime_type, ..
            } => {
                assert!(text.starts_with("# tool"));
                assert_eq!(mime_type.as_deref(), Some("text/tab-separated-values"));
            }
            other => panic!("unexpected contents: {other:?}"),
        }
    }

    #[test]
    fn an_unknown_resource_uri_is_a_not_found() {
        let mcp = bounded_mcp(small_policy(4096));
        assert!(mcp.read_resource_uri("gitea-response:/nothing/0").is_err());
    }

    #[test]
    fn the_index_is_listed_once_across_a_full_traversal() {
        let mcp = bounded_mcp(small_policy(4096));
        for index in 0..(RESOURCE_PAGE_SIZE + 10) {
            mcp.store
                .insert(&format!("op{index}"), "text/plain", false, vec![b'x'; 8])
                .expect("stored");
        }

        let first = mcp.list_resources_page(None);
        let cursor = first.next_cursor.clone().expect("more remains");
        let second = mcp.list_resources_page(Some(&cursor));

        let count = |page: &rmcp::model::ListResourcesResult| {
            page.resources
                .iter()
                .filter(|resource| resource.uri == CATALOG_INDEX_URI)
                .count()
        };
        assert_eq!(count(&first), 1, "the cursorless page carries it");
        assert_eq!(count(&second), 0, "the terminal page does not repeat it");
        assert!(second.next_cursor.is_none());
    }
}
