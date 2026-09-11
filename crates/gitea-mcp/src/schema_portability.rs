use std::sync::Arc;

use rmcp::model::Tool;
use serde_json::{Map, Value, json};

const BOOLEAN_VALUED_KEYWORDS: &[&str] = &[
    "additionalProperties",
    "unevaluatedProperties",
    "additionalItems",
    "unevaluatedItems",
];

const SUBSCHEMA_KEYWORDS: &[&str] = &[
    "items",
    "contains",
    "not",
    "propertyNames",
    "if",
    "then",
    "else",
    "additionalProperties",
    "unevaluatedProperties",
    "additionalItems",
    "unevaluatedItems",
    "contentSchema",
];

const SUBSCHEMA_ARRAY_KEYWORDS: &[&str] = &["allOf", "anyOf", "oneOf", "prefixItems"];

const SUBSCHEMA_MAP_KEYWORDS: &[&str] = &[
    "properties",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
    "$defs",
    "definitions",
];

const CONSTRAINING_KEYWORDS: &[&str] = &[
    "type",
    "enum",
    "const",
    "multipleOf",
    "maximum",
    "exclusiveMaximum",
    "minimum",
    "exclusiveMinimum",
    "maxLength",
    "minLength",
    "pattern",
    "format",
    "contentMediaType",
    "contentEncoding",
    "contentSchema",
    "maxItems",
    "minItems",
    "uniqueItems",
    "maxContains",
    "minContains",
    "maxProperties",
    "minProperties",
    "required",
    "dependentRequired",
    "allOf",
    "anyOf",
    "oneOf",
    "not",
    "items",
    "prefixItems",
    "contains",
    "additionalItems",
    "unevaluatedItems",
    "properties",
    "patternProperties",
    "additionalProperties",
    "unevaluatedProperties",
    "propertyNames",
    "dependentSchemas",
    "dependencies",
    "$ref",
    "$dynamicRef",
    "$recursiveRef",
];

const JSON_SCHEMA_TYPES: &[&str] = &[
    "null", "boolean", "object", "array", "number", "string", "integer",
];

/// Rewrite legal but unevenly-supported JSON Schema spellings at the MCP
/// publication boundary without changing the values the schema accepts.
pub(crate) fn normalize_tool(mut tool: Tool) -> Tool {
    let mut input = Value::Object(tool.input_schema.as_ref().clone());
    normalize_schema(&mut input, None, 0);
    tool.input_schema = Arc::new(
        input
            .as_object()
            .expect("an MCP input schema has an object root")
            .clone(),
    );

    if let Some(output_schema) = tool.output_schema.take() {
        let mut output = Value::Object(output_schema.as_ref().clone());
        normalize_schema(&mut output, None, 0);
        tool.output_schema = Some(Arc::new(
            output
                .as_object()
                .expect("an MCP output schema has an object root")
                .clone(),
        ));
    }

    tool
}

fn normalize_schema(node: &mut Value, parent_keyword: Option<&str>, depth: usize) {
    if depth > 64 {
        return;
    }

    if let Value::Bool(value) = node {
        if parent_keyword.is_some_and(|keyword| BOOLEAN_VALUED_KEYWORDS.contains(&keyword)) {
            return;
        }
        *node = if *value {
            any_json_schema()
        } else {
            json!({"not": {}})
        };
    }

    let Value::Object(schema) = node else {
        return;
    };

    normalize_type_union(schema);
    if !constrains_instance(schema) && parent_keyword != Some("not") {
        schema.insert("anyOf".to_string(), any_json_types());
    }

    for keyword in SUBSCHEMA_MAP_KEYWORDS {
        let Some(Value::Object(children)) = schema.get_mut(*keyword) else {
            continue;
        };
        for child in children.values_mut() {
            normalize_schema(child, Some(keyword), depth + 1);
        }
    }

    for keyword in SUBSCHEMA_ARRAY_KEYWORDS {
        let Some(Value::Array(children)) = schema.get_mut(*keyword) else {
            continue;
        };
        for child in children {
            normalize_schema(child, Some(keyword), depth + 1);
        }
    }

    for keyword in SUBSCHEMA_KEYWORDS {
        let Some(child) = schema.get_mut(*keyword) else {
            continue;
        };
        if *keyword == "items"
            && let Value::Array(children) = child
        {
            for item in children {
                normalize_schema(item, Some(keyword), depth + 1);
            }
        } else {
            normalize_schema(child, Some(keyword), depth + 1);
        }
    }
}

fn normalize_type_union(schema: &mut Map<String, Value>) {
    let Some(Value::Array(types)) = schema.get("type") else {
        return;
    };
    if types.is_empty()
        || types.iter().any(|kind| {
            kind.as_str()
                .is_none_or(|kind| !JSON_SCHEMA_TYPES.contains(&kind))
        })
    {
        return;
    }
    let mut distinct = std::collections::BTreeSet::new();
    if !types
        .iter()
        .all(|kind| distinct.insert(kind.as_str().expect("checked above")))
    {
        return;
    }

    let branches = types.iter().map(|kind| json!({"type": kind})).collect();
    schema.remove("type");
    let union = json!({"anyOf": Value::Array(branches)});
    if schema.contains_key("anyOf") {
        schema
            .entry("allOf")
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .expect("a compiled schema has an array-valued allOf")
            .push(union);
    } else {
        schema.insert("anyOf".to_string(), union["anyOf"].clone());
    }
}

fn constrains_instance(schema: &Map<String, Value>) -> bool {
    schema
        .keys()
        .any(|keyword| CONSTRAINING_KEYWORDS.contains(&keyword.as_str()))
        || (schema.contains_key("if")
            && (schema.contains_key("then") || schema.contains_key("else")))
}

fn any_json_schema() -> Value {
    Value::Object(Map::from_iter([("anyOf".to_string(), any_json_types())]))
}

fn any_json_types() -> Value {
    Value::Array(
        JSON_SCHEMA_TYPES
            .iter()
            .map(|kind| json!({"type": kind}))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inspector_findings(schema: &Value) -> Vec<String> {
        let mut findings = Vec::new();
        let has_embedded_ids = declares_any_id(schema, 0);
        inspect(schema, "", None, 0, has_embedded_ids, &mut findings);
        findings
    }

    fn declares_any_id(node: &Value, depth: usize) -> bool {
        if depth > 64 {
            return false;
        }
        let Value::Object(schema) = node else {
            return false;
        };
        if schema
            .get("$id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty())
        {
            return true;
        }
        let mut children = Vec::new();
        subschemas(schema, |child, _| children.push(child));
        children
            .into_iter()
            .any(|child| declares_any_id(child, depth + 1))
    }

    fn inspect(
        node: &Value,
        path: &str,
        parent_keyword: Option<&str>,
        depth: usize,
        has_embedded_ids: bool,
        findings: &mut Vec<String>,
    ) {
        if depth > 64 {
            return;
        }
        if node.is_boolean() {
            if !parent_keyword.is_some_and(|keyword| BOOLEAN_VALUED_KEYWORDS.contains(&keyword)) {
                findings.push(format!("boolean-schema at {path}"));
            }
            return;
        }
        let Value::Object(schema) = node else {
            return;
        };

        if schema
            .get("type")
            .and_then(Value::as_array)
            .is_some_and(|types| {
                !types.is_empty()
                    && types.iter().all(|kind| {
                        kind.as_str()
                            .is_some_and(|kind| JSON_SCHEMA_TYPES.contains(&kind))
                    })
                    && types
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<std::collections::BTreeSet<_>>()
                        .len()
                        == types.len()
            })
        {
            findings.push(format!("type-union at {path}"));
        }
        if schema
            .get("$ref")
            .and_then(Value::as_str)
            .is_some_and(|reference| !reference.is_empty() && !reference.starts_with('#'))
            && !has_embedded_ids
        {
            findings.push(format!("remote-ref at {path}"));
        }
        if !constrains_instance(schema) && parent_keyword != Some("not") {
            findings.push(format!("untyped-schema at {path}"));
        }

        subschemas(schema, |child, keyword| {
            let child_path = if path.is_empty() {
                keyword.to_string()
            } else {
                format!("{path}.{keyword}")
            };
            inspect(
                child,
                &child_path,
                Some(keyword),
                depth + 1,
                has_embedded_ids,
                findings,
            );
        });
    }

    fn subschemas<'a>(
        schema: &'a Map<String, Value>,
        mut visit: impl FnMut(&'a Value, &'static str),
    ) {
        for &keyword in SUBSCHEMA_MAP_KEYWORDS {
            if let Some(Value::Object(children)) = schema.get(keyword) {
                for child in children.values() {
                    visit(child, keyword);
                }
            }
        }
        for &keyword in SUBSCHEMA_ARRAY_KEYWORDS {
            if let Some(Value::Array(children)) = schema.get(keyword) {
                for child in children {
                    visit(child, keyword);
                }
            }
        }
        for &keyword in SUBSCHEMA_KEYWORDS {
            let Some(child) = schema.get(keyword) else {
                continue;
            };
            if keyword == "items"
                && let Value::Array(children) = child
            {
                for item in children {
                    visit(item, keyword);
                }
            } else {
                visit(child, keyword);
            }
        }
    }

    #[test]
    fn every_published_schema_satisfies_the_inspector_portability_rules() {
        let mut findings = Vec::new();
        for tool in crate::GiteaMcp::list_tools_payload().tools {
            for (kind, schema) in [
                ("input", Some(tool.input_schema.as_ref())),
                ("output", tool.output_schema.as_deref()),
            ] {
                let Some(schema) = schema else {
                    continue;
                };
                for finding in inspector_findings(&Value::Object(schema.clone())) {
                    findings.push(format!("{}.{}: {finding}", tool.name, kind));
                }
            }
        }
        assert!(
            findings.is_empty(),
            "published schemas have Inspector portability findings: {findings:#?}"
        );
    }

    #[test]
    fn normalization_preserves_universal_and_impossible_schemas() {
        let mut universal = Value::Bool(true);
        normalize_schema(&mut universal, Some("properties"), 0);
        let universal = jsonschema::draft4::new(&universal).expect("universal schema compiles");

        let mut impossible = Value::Bool(false);
        normalize_schema(&mut impossible, Some("properties"), 0);
        let impossible = jsonschema::draft4::new(&impossible).expect("false schema compiles");

        for value in [
            Value::Null,
            json!(true),
            json!({}),
            json!([]),
            json!(1),
            json!(1.5),
            json!("value"),
        ] {
            assert!(universal.is_valid(&value), "universal rejected {value}");
            assert!(!impossible.is_valid(&value), "false accepted {value}");
        }
    }

    #[test]
    fn normalization_preserves_nullable_union_semantics() {
        let mut schema = json!({"type": ["string", "null"], "minLength": 2});
        normalize_schema(&mut schema, Some("properties"), 0);
        let validator = jsonschema::draft4::new(&schema).expect("nullable schema compiles");

        assert!(validator.is_valid(&Value::Null));
        assert!(validator.is_valid(&json!("ok")));
        assert!(!validator.is_valid(&json!("x")));
        assert!(!validator.is_valid(&json!(1)));
    }

    #[test]
    fn published_generated_results_still_accept_every_json_payload_type() {
        let tool = crate::GiteaMcp::list_tools_payload()
            .tools
            .into_iter()
            .find(|tool| tool.name == crate::lanes::READ_TOOL)
            .expect("api.read is published");
        let schema = Value::Object(
            tool.output_schema
                .expect("api.read has an output schema")
                .as_ref()
                .clone(),
        );
        let validator = jsonschema::draft4::new(&schema).expect("lane output schema compiles");

        for data in [
            Value::Null,
            json!(true),
            json!({"id": 1}),
            json!([1, 2]),
            json!(1),
            json!(1.5),
            json!("value"),
        ] {
            let result = json!({
                "operation_id": "repoGet",
                "status": 200,
                "success": true,
                "content_type": "application/json",
                "headers": {},
                "data": data,
            });
            assert!(
                validator.is_valid(&result),
                "published lane schema rejected runtime result {result}"
            );
        }
    }
}
