//! Execution lanes: four dispatch tools that execute any catalog operation by
//! its identity, one lane per risk class.
//!
//! A lane call names its operation explicitly and is validated against that
//! operation's generated closed schema — exactly the validation the flat tool
//! carries — so collapsing hundreds of names into four tools loses no
//! precision and hides nothing from policy: the executed operation's identity,
//! risk, and flags ride in the call result's metadata channel.
//!
//! Lane routing is an enforced invariant, not advice. An administrative
//! operation executes only through `api.admin`; everything else only through
//! the lane matching its generated risk class. A mismatch is a structured
//! invalid-params error naming the correct lane, never a silent redirect, so
//! neither a caller nor an audit line can launder a destructive call through
//! a read lane.
//!
//! The lanes are the only way to execute a catalog operation: the operation
//! name is not accepted as a tool of its own, so nothing can route around the
//! classification the lanes enforce.

use std::borrow::Cow;
use std::sync::Arc;

use gitea_api::catalog::{
    OperationRisk, OperationSpec, exposed_operation, exposed_operation_by_id, operation_catalog,
};
use rmcp::ErrorData as McpError;
use rmcp::model::{Meta, Tool, ToolAnnotations};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::SENSITIVE_RESULT_META;
use crate::discovery::{MAX_NAME_CHARS, exceeds_chars, nearest_names};
use crate::json_object_schema;

pub const READ_TOOL: &str = "api.read";
pub const MUTATE_TOOL: &str = "api.mutate";
pub const DESTROY_TOOL: &str = "api.destroy";
pub const ADMIN_TOOL: &str = "api.admin";

/// The four lanes, in the order they are published.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Read,
    Mutate,
    Destroy,
    Admin,
}

impl Lane {
    pub const fn tool_name(self) -> &'static str {
        match self {
            Lane::Read => READ_TOOL,
            Lane::Mutate => MUTATE_TOOL,
            Lane::Destroy => DESTROY_TOOL,
            Lane::Admin => ADMIN_TOOL,
        }
    }

    pub fn from_tool_name(name: &str) -> Option<Self> {
        match name {
            READ_TOOL => Some(Lane::Read),
            MUTATE_TOOL => Some(Lane::Mutate),
            DESTROY_TOOL => Some(Lane::Destroy),
            ADMIN_TOOL => Some(Lane::Admin),
            _ => None,
        }
    }
}

/// The lane an operation belongs to. Administrative wins over risk, so every
/// operation has exactly one lane.
pub fn lane_of(operation: &OperationSpec) -> Lane {
    if operation.administrative {
        return Lane::Admin;
    }
    match operation.risk {
        OperationRisk::Read => Lane::Read,
        OperationRisk::Mutation => Lane::Mutate,
        OperationRisk::Destructive => Lane::Destroy,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneArguments {
    pub operation_id: String,
    #[serde(default)]
    pub arguments: Map<String, Value>,
}

/// Resolve a lane call to its operation, enforcing routing.
///
/// # Errors
///
/// Returns invalid-params for an oversized or unknown identifier — the
/// unknown case carries the nearest registered identifiers — and for an
/// operation that belongs to a different lane, naming the lane it belongs to.
pub fn resolve(lane: Lane, arguments: &LaneArguments) -> Result<&'static OperationSpec, McpError> {
    if exceeds_chars(&arguments.operation_id, MAX_NAME_CHARS) {
        return Err(McpError::invalid_params(
            format!("operation_id exceeds {MAX_NAME_CHARS} characters"),
            None,
        ));
    }
    let operation = exposed_operation(&arguments.operation_id)
        .or_else(|| exposed_operation_by_id(&arguments.operation_id));
    let Some(operation) = operation else {
        // A lane name is not an operation; self-referential "call it
        // directly" guidance would send the caller in a circle.
        if Lane::from_tool_name(&arguments.operation_id).is_some() {
            return Err(McpError::invalid_params(
                format!(
                    "{} is a lane, not an operation; pass a catalog operation id",
                    arguments.operation_id
                ),
                None,
            ));
        }
        // A hand-written tool is called directly in every mode; discovery
        // lists it beside the catalog operations, so a layered caller can
        // reasonably hand its name to a lane, and pointing home is the
        // actionable answer.
        let non_generated = crate::GiteaMcp::list_tools_payload();
        if let Some(tool) = non_generated.tools.iter().find(|tool| {
            tool.name == arguments.operation_id
                || tool
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.0.get("org.cacahuate/operationId"))
                    .and_then(Value::as_str)
                    == Some(arguments.operation_id.as_str())
        }) {
            return Err(McpError::invalid_params(
                format!(
                    "{} is the tool {}, not a catalog operation; call it directly in any mode",
                    arguments.operation_id, tool.name
                ),
                None,
            ));
        }
        // A ledger-replaced operation is served by a hand-written tool, not
        // the catalog; pointing at the replacement is the actionable answer,
        // where a bare unknown would send the caller hunting.
        if let Some(entry) = operation_catalog()
            .compatibility_ledger
            .iter()
            .find(|entry| {
                entry.operation_id == arguments.operation_id
                    || entry.tool_name == arguments.operation_id
                    || entry.replacement.as_deref() == Some(arguments.operation_id.as_str())
            })
        {
            let replacement = entry
                .replacement
                .as_deref()
                .unwrap_or("a hand-written tool");
            return Err(McpError::invalid_params(
                format!(
                    "{} is served by the hand-written tool {replacement}; call it directly",
                    arguments.operation_id
                ),
                None,
            ));
        }
        // Deprecated operations stay exact-name-only: suggesting one would
        // reintroduce the near-duplicate names their exclusion exists to
        // remove.
        let candidates: Vec<&str> = operation_catalog()
            .operations
            .iter()
            .filter(|operation| operation.exposed && !operation.deprecated.is_deprecated())
            .flat_map(|operation| {
                [
                    operation.tool_name.as_str(),
                    operation.operation_id.as_str(),
                ]
            })
            .collect();
        return Err(McpError::invalid_params(
            format!(
                "no operation named {}; nearest: {}",
                arguments.operation_id,
                nearest_names(&arguments.operation_id, &candidates).join(", ")
            ),
            None,
        ));
    };
    let owner = lane_of(operation);
    if owner != lane {
        return Err(McpError::invalid_params(
            format!(
                "{} belongs to {}, not {}; call it there",
                operation.tool_name,
                owner.tool_name(),
                lane.tool_name()
            ),
            None,
        ));
    }
    Ok(operation)
}

/// The resolved operation's identity, merged into a failed lane call's error
/// data so audit sees which operation may have run even when no result
/// exists to carry metadata.
pub fn identity_error_data(operation: &OperationSpec, data: Option<Value>) -> Value {
    let mut fields = match data {
        Some(Value::Object(fields)) => fields,
        Some(other) => Map::from_iter([("detail".to_string(), other)]),
        None => Map::new(),
    };
    fields.insert(
        "operation_id".to_string(),
        Value::String(operation.operation_id.clone()),
    );
    fields.insert(
        "risk".to_string(),
        Value::String(
            match operation.risk {
                OperationRisk::Read => "read",
                OperationRisk::Mutation => "mutation",
                OperationRisk::Destructive => "destructive",
            }
            .to_string(),
        ),
    );
    fields.insert(
        "administrative".to_string(),
        Value::Bool(operation.administrative),
    );
    Value::Object(fields)
}

/// The executed operation's identity, merged into a lane result's metadata so
/// audit and policy see the true operation behind the shared lane name.
pub fn identity_meta(operation: &OperationSpec, meta: Option<Meta>) -> Meta {
    let mut fields = meta.map(|meta| meta.0).unwrap_or_default();
    fields.insert(
        "org.cacahuate/operationId".to_string(),
        Value::String(operation.operation_id.clone()),
    );
    fields.insert(
        "org.cacahuate/risk".to_string(),
        Value::String(
            match operation.risk {
                OperationRisk::Read => "read",
                OperationRisk::Mutation => "mutation",
                OperationRisk::Destructive => "destructive",
            }
            .to_string(),
        ),
    );
    fields.insert(
        "org.cacahuate/administrative".to_string(),
        Value::Bool(operation.administrative),
    );
    Meta(fields)
}

pub fn tools() -> [Tool; 4] {
    [
        lane_tool(
            Lane::Read,
            "Execute one read-only catalog operation by tool name or upstream operation id. \
             Find identifiers with catalog.search and their exact argument schema with \
             catalog.describe; arguments are validated against that schema before anything \
             is sent. Read-only operations only — other lanes refuse them by name.",
        ),
        lane_tool(
            Lane::Mutate,
            "Execute one non-destructive mutating catalog operation by tool name or upstream \
             operation id. Find identifiers with catalog.search and their exact argument \
             schema with catalog.describe; arguments are validated against that schema \
             before anything is sent. Mutates Gitea.",
        ),
        lane_tool(
            Lane::Destroy,
            "Execute one destructive catalog operation by tool name or upstream operation \
             id. Find identifiers with catalog.search and their exact argument schema with \
             catalog.describe; arguments are validated against that schema before anything \
             is sent. Destructive Gitea mutation.",
        ),
        lane_tool(
            Lane::Admin,
            "Execute one instance-administration catalog operation by tool name or upstream \
             operation id, whatever its risk class. Find identifiers with catalog.search \
             and their exact argument schema with catalog.describe; arguments are validated \
             against that schema before anything is sent. Administrative; may mutate or \
             destroy instance state.",
        ),
    ]
}

/// One lane tool, its hints derived from the operations it actually routes so
/// a specification regeneration cannot leave them stale.
fn lane_tool(lane: Lane, description: &'static str) -> Tool {
    let members = || {
        operation_catalog()
            .operations
            .iter()
            .filter(move |operation| operation.exposed && lane_of(operation) == lane)
    };
    let open_world = members().any(|operation| operation.open_world.is_enabled());
    let sensitive_capable = members().any(|operation| operation.secret_result);
    let (read_only, destructive, idempotent, risk_name) = match lane {
        Lane::Read => (true, false, true, "read"),
        Lane::Mutate => (false, false, false, "mutation"),
        // The admin lane shares the destroy lane's worst case: it routes
        // destructive administrative operations.
        Lane::Destroy | Lane::Admin => (false, true, false, "destructive"),
    };
    let mut tool = Tool::new(
        Cow::Borrowed(lane.tool_name()),
        Cow::Borrowed(description),
        Arc::new(json_object_schema(
            json!({
                "operation_id": {
                    "type": "string",
                    "maxLength": MAX_NAME_CHARS,
                    "description": "tool name such as repository.get, or upstream operation \
                                    id such as repoGet"
                },
                "arguments": {
                    "type": "object",
                    "description": "the operation's arguments, exactly as catalog.describe \
                                    publishes their schema"
                }
            }),
            &["operation_id"],
        )),
    )
    .with_annotations(
        ToolAnnotations::new()
            .read_only(read_only)
            .destructive(destructive)
            .idempotent(idempotent)
            .open_world(open_world),
    );
    // A lane result IS a generated result — same executor, same envelope,
    // same displacement — so it advertises exactly the generated output
    // schema rather than a lane-local restatement that could drift from it.
    tool.output_schema = Some(Arc::new(crate::generated_output_schema()));
    // The metadata channel carries the lane's worst case; the per-call result
    // metadata carries the executed operation's exact identity and the
    // per-result sensitivity marker stays authoritative for handling.
    tool.meta = Some(Meta(Map::from_iter([
        (
            "org.cacahuate/risk".to_string(),
            Value::String(risk_name.to_string()),
        ),
        (
            "org.cacahuate/administrative".to_string(),
            Value::Bool(lane == Lane::Admin),
        ),
        (
            SENSITIVE_RESULT_META.to_string(),
            Value::Bool(sensitive_capable),
        ),
    ])));
    tool
}
