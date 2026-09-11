use std::{borrow::Cow, sync::Arc};

use rmcp::model::{Meta, Tool, ToolAnnotations};
use serde::Deserialize;
use serde_json::{Map, Value, json};

pub const TOOL_NAME: &str = "repository.secret.set_from_file";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Arguments {
    pub(super) owner: String,
    pub(super) repo: String,
    pub(super) name: String,
    pub(super) data_file: String,
}

impl Arguments {
    pub(super) fn validate_target(&self) -> Result<(), &'static str> {
        if [&self.owner, &self.repo, &self.name]
            .into_iter()
            .any(|value| value.is_empty() || value.chars().count() > 255)
        {
            return Err("owner, repo, and name must contain 1 to 255 characters");
        }
        Ok(())
    }
}

pub fn tool() -> Tool {
    let mut tool = Tool::new(
        Cow::Borrowed(TOOL_NAME),
        Cow::Borrowed(
            "Set or replace one repository Actions secret from a governed uploaded file. The secret value is consumed once and never returned.",
        ),
        Arc::new(
            json!({
                "type": "object",
                "properties": {
                    "owner": {"type": "string", "minLength": 1, "maxLength": 255},
                    "repo": {"type": "string", "minLength": 1, "maxLength": 255},
                    "name": {"type": "string", "minLength": 1, "maxLength": 255},
                    "data_file": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 512,
                        "x-mcp-file": {
                            "transferModes": ["upload"],
                            "maxSize": crate::files::MAX_SECRET_BYTES
                        }
                    }
                },
                "required": ["owner", "repo", "name", "data_file"],
                "additionalProperties": false
            })
            .as_object()
            .expect("repository secret schema")
            .clone(),
        ),
    )
    .with_annotations(
        ToolAnnotations::new()
            .read_only(false)
            .destructive(true)
            .idempotent(true)
            .open_world(true),
    );
    tool.output_schema = Some(Arc::new(crate::displaceable_output_schema(
        json!({
                "operation_id": {"type": "string"},
                "status": {"type": "integer"},
                "success": {"type": "boolean"},
                "content_type": {"type": ["string", "null"]},
                "headers": {"type": "object"},
                "data": {}
        }),
        &[
            "operation_id",
            "status",
            "success",
            "content_type",
            "headers",
            "data",
        ],
    )));
    tool.meta = Some(Meta(Map::from_iter([
        (
            "org.cacahuate/operationId".to_owned(),
            Value::String("updateRepoSecret".to_owned()),
        ),
        (
            "org.cacahuate/risk".to_owned(),
            Value::String("destructive".to_owned()),
        ),
        (
            "org.cacahuate/administrative".to_owned(),
            Value::Bool(false),
        ),
        (crate::SENSITIVE_RESULT_META.to_owned(), Value::Bool(false)),
        ("org.cacahuate/sensitiveInput".to_owned(), Value::Bool(true)),
    ])));
    tool
}
