//! Typed, bounded views over an immutable retained payload.

use rmcp::{ErrorData as McpError, model::CallToolResult};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::resources::StoredResource;

pub const TOOL: &str = "result.select";
const MAX_REPLY_BYTES: usize = 8 * 1024;
const MAX_JSON_BYTES: usize = 2 * 1024 * 1024;
const MAX_WINDOW_BYTES: usize = 4096;

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Text,
    Search,
    Json,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Arguments {
    pub uri: String,
    pub mode: Mode,
    #[serde(default)]
    pub offset: usize,
    pub limit: Option<usize>,
    pub text: Option<String>,
    pub context_lines: Option<usize>,
    pub pointer: Option<String>,
    pub fields: Option<Vec<String>>,
}

fn invalid(message: &'static str) -> McpError {
    McpError::invalid_params(message, None)
}

pub fn tool() -> rmcp::model::Tool {
    use rmcp::model::{Meta, Tool, ToolAnnotations};
    use std::{borrow::Cow, sync::Arc};
    let mut tool = Tool::new(
        Cow::Borrowed(TOOL),
        Cow::Borrowed(
            "Read a bounded selection from a retained gitea-response URI owned by the authenticated identity. Use text for UTF-8 byte ranges, search for a literal with surrounding lines, or json for an RFC 6901 pointer, rows and exact object fields. Resume with next_offset. Replies stay within 8 KiB; JSON parsing is limited to 2 MiB. Complete payloads remain available through resources/read and authorized downloads.",
        ),
        Arc::new(crate::json_object_schema(
            json!({
                "uri":{"type":"string","minLength":1,"maxLength":512},
                "mode":{"type":"string","enum":["text","search","json"]},
                "offset":{"type":"integer","minimum":0,"description":"UTF-8 byte offset for text/search, or row offset for a JSON array."},
                "limit":{"type":"integer","minimum":1,"maximum":4096,"description":"Maximum selection bytes for text/search, or rows (at most 100) for JSON; the reply byte bound may reduce this."},
                "text":{"type":"string","minLength":1,"maxLength":128,"description":"Search-only literal, at most 128 UTF-8 bytes; no regular expressions."},
                "context_lines":{"type":"integer","minimum":0,"maximum":10},
                "pointer":{"type":"string","maxLength":512,"description":"JSON-only pointer, at most 512 UTF-8 bytes; empty selects the root."},
                "fields":{"type":"array","maxItems":16,"items":{"type":"string","maxLength":128},"description":"Exact JSON object field names, each at most 128 UTF-8 bytes."}
            }),
            &["uri", "mode"],
        )),
    );
    tool.annotations = Some(
        ToolAnnotations::new()
            .read_only(true)
            .destructive(false)
            .idempotent(true)
            .open_world(false),
    );
    tool.meta = Some(Meta(Map::from_iter([
        ("org.cacahuate/administrative".into(), json!(false)),
        (crate::SENSITIVE_RESULT_META.into(), json!(true)),
    ])));
    tool.output_schema = Some(Arc::new(crate::json_object_schema(
        json!({
            "uri":{"type":"string"},"source_bytes":{"type":"integer","minimum":0},
            "selection":{"type":"object","properties":{
                "data":{},"start":{"type":"integer","minimum":0},"end":{"type":"integer","minimum":0},
                "unit":{"type":"string","enum":["bytes","rows"]},
                "next_offset":{"type":["integer","null"],"minimum":0},
                "complete":{"type":"boolean"},"truncated":{"type":"boolean"},
                "matched":{"type":"boolean"},"total_rows":{"type":"integer","minimum":0}
            },"required":["data","start","end","unit","next_offset","complete","truncated"],"additionalProperties":false}
        }),
        &["uri", "source_bytes", "selection"],
    )));
    tool
}

impl Arguments {
    pub fn validate(&self) -> Result<(), McpError> {
        if self.uri.len() > 512
            || self
                .limit
                .is_some_and(|limit| limit == 0 || limit > MAX_WINDOW_BYTES)
            || self.context_lines.is_some_and(|lines| lines > 10)
            || self
                .text
                .as_ref()
                .is_some_and(|text| text.is_empty() || text.len() > 128)
            || self
                .pointer
                .as_ref()
                .is_some_and(|pointer| pointer.len() > 512)
            || self.fields.as_ref().is_some_and(|fields| {
                fields.len() > 16 || fields.iter().any(|field| field.len() > 128)
            })
        {
            return Err(invalid("selection exceeds its argument bounds"));
        }
        let valid = match self.mode {
            Mode::Text => {
                self.text.is_none()
                    && self.context_lines.is_none()
                    && self.pointer.is_none()
                    && self.fields.is_none()
            }
            Mode::Search => self.text.is_some() && self.pointer.is_none() && self.fields.is_none(),
            Mode::Json => {
                self.text.is_none()
                    && self.context_lines.is_none()
                    && self.limit.is_none_or(|limit| limit <= 100)
            }
        };
        if !valid {
            return Err(invalid(
                "selection arguments do not apply to the selected mode",
            ));
        }
        Ok(())
    }
}

pub fn select(
    resource: &StoredResource,
    arguments: &Arguments,
    ceiling: usize,
) -> Result<CallToolResult, McpError> {
    arguments.validate()?;
    let mut limit = arguments.limit.unwrap_or(match arguments.mode {
        Mode::Json => 20,
        _ => MAX_WINDOW_BYTES,
    });
    let parsed = if matches!(arguments.mode, Mode::Json) {
        if !crate::declares_json(&resource.content_type) || resource.body.len() > MAX_JSON_BYTES {
            return Err(invalid(
                "JSON selection requires a JSON payload within 2 MiB; use the complete download for larger objects",
            ));
        }
        Some(
            serde_json::from_slice::<Value>(&resource.body)
                .map_err(|_| invalid("retained payload is not valid JSON"))?,
        )
    } else {
        None
    };
    loop {
        let selected = match arguments.mode {
            Mode::Text | Mode::Search => text_selection(&resource.body, arguments, limit)?,
            Mode::Json => json_selection(parsed.as_ref().expect("parsed JSON"), arguments, limit)?,
        };
        let value =
            json!({"uri":resource.uri,"source_bytes":resource.body.len(),"selection":selected});
        let mut result = CallToolResult::structured(value);
        result.meta = crate::result_meta(resource.sensitive, None);
        result.is_error = Some(false);
        let bytes = serde_json::to_vec(&result)
            .map_err(|_| McpError::internal_error("failed to encode selection", None))?;
        if bytes.len() <= MAX_REPLY_BYTES.min(ceiling) {
            return Ok(result);
        }
        if limit <= 1 {
            return Err(invalid(
                "selected item exceeds the reply bound; narrow its JSON pointer or fields, or download the complete payload",
            ));
        }
        limit /= 2;
    }
}

fn text_selection(bytes: &[u8], args: &Arguments, limit: usize) -> Result<Value, McpError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| invalid("text selection requires UTF-8; download binary payloads"))?;
    if !text.is_char_boundary(args.offset) {
        return Err(invalid(
            "offset must be a UTF-8 byte boundary within the payload",
        ));
    }
    let mut start = args.offset;
    let mut end = text.len().min(start.saturating_add(limit));
    let mut next = None;
    let mut truncated = end < text.len();
    if let Some(needle) = &args.text {
        let Some(relative) = text[start..].find(needle) else {
            return Ok(
                json!({"data":"","start":start,"end":start,"unit":"bytes","next_offset":null,"complete":true,"truncated":false,"matched":false}),
            );
        };
        let found = start + relative;
        let after = found + needle.len();
        if limit < needle.len() {
            return Err(invalid(
                "search match exceeds the reply bound; use a shorter literal",
            ));
        }
        start = text[..found].rfind('\n').map_or(0, |index| index + 1);
        end = text[after..]
            .find('\n')
            .map_or(text.len(), |index| after + index + 1);
        for _ in 0..args.context_lines.unwrap_or(2) {
            if start > 0 {
                start = text[..start - 1].rfind('\n').map_or(0, |index| index + 1);
            }
            end = text[end..]
                .find('\n')
                .map_or(text.len(), |index| end + index + 1);
            if start == 0 && end == text.len() {
                break;
            }
        }
        let left = found.saturating_sub((limit - needle.len()) / 2);
        let bounded_start = start.max(left);
        truncated = bounded_start > start || end > bounded_start.saturating_add(limit);
        start = bounded_start;
        end = end.min(start.saturating_add(limit));
        next = (after < text.len()).then_some(after);
    }
    while !text.is_char_boundary(start) {
        start += 1;
    }
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if args.text.is_none() {
        if start == end && start < text.len() {
            return Err(invalid("limit must fit at least one UTF-8 character"));
        }
        next = (end < text.len()).then_some(end);
    }
    Ok(
        json!({"data":&text[start..end],"start":start,"end":end,"unit":"bytes","next_offset":next,"complete":next.is_none(),"truncated":truncated,"matched":args.text.is_some()}),
    )
}

fn json_selection(value: &Value, args: &Arguments, limit: usize) -> Result<Value, McpError> {
    let selected = value
        .pointer(args.pointer.as_deref().unwrap_or(""))
        .ok_or_else(|| invalid("JSON pointer does not identify a value"))?;
    let project = |value: &Value| -> Result<Value, McpError> {
        let Some(fields) = &args.fields else {
            return Ok(value.clone());
        };
        let object = value
            .as_object()
            .ok_or_else(|| invalid("field selection requires JSON objects"))?;
        Ok(Value::Object(
            fields
                .iter()
                .filter_map(|key| object.get(key).map(|value| (key.clone(), value.clone())))
                .collect::<Map<_, _>>(),
        ))
    };
    let (data, end, total) = if let Some(rows) = selected.as_array() {
        if args.offset > rows.len() {
            return Err(invalid("row offset exceeds the selected array"));
        }
        let end = rows.len().min(args.offset.saturating_add(limit));
        let data = rows[args.offset..end]
            .iter()
            .map(project)
            .collect::<Result<Vec<_>, _>>()?;
        (json!(data), end, rows.len())
    } else {
        if args.offset != 0 {
            return Err(invalid("row offset only applies to arrays"));
        }
        (project(selected)?, 1, 1)
    };
    let next = (end < total).then_some(end);
    Ok(
        json!({"data":data,"start":args.offset,"end":end,"total_rows":total,"unit":"rows","next_offset":next,"complete":next.is_none(),"truncated":next.is_some()}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn select(
        resource: &StoredResource,
        arguments: &Arguments,
    ) -> Result<CallToolResult, McpError> {
        super::select(resource, arguments, MAX_REPLY_BYTES)
    }

    #[test]
    fn selection_honors_a_smaller_configured_context_ceiling() {
        let resource = resource(b"\"\\\n".repeat(10_000), "text/plain");
        let result = super::select(
            &resource,
            &arguments(json!({"uri":resource.uri,"mode":"text"})),
            4096,
        )
        .unwrap();
        assert!(serde_json::to_vec(&result).unwrap().len() <= 4096);
        assert!(result.structured_content.unwrap()["selection"]["next_offset"].is_number());
    }

    fn resource(body: Vec<u8>, media: &str) -> StoredResource {
        StoredResource {
            uri: "gitea-response:/fixture/0".into(),
            operation_id: "fixture".into(),
            content_type: media.into(),
            sensitive: true,
            body: body.into(),
        }
    }

    fn arguments(value: Value) -> Arguments {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn a_failure_near_the_end_of_ten_mib_fits_the_evidence_budget() {
        let mut body = b"successful build line\n".repeat(500_000);
        body.extend_from_slice(b"FAILURE: compilation stopped\nlast context line\n");
        let resource = resource(body, "text/plain");
        let result = select(
            &resource,
            &arguments(json!({"uri":resource.uri,"mode":"search","text":"FAILURE:"})),
        )
        .unwrap();
        assert!(resource.body.len() >= 10 * 1024 * 1024);
        assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_REPLY_BYTES);
        assert!(
            result.structured_content.unwrap()["selection"]["data"]
                .as_str()
                .unwrap()
                .contains("FAILURE: compilation stopped")
        );
        assert_eq!(result.meta.unwrap().0[crate::SENSITIVE_RESULT_META], true);
    }

    #[test]
    fn literal_search_context_and_continuation_are_stable() {
        let resource = resource(b"one\nmatch\nthree\nmatch\nfive\n".to_vec(), "text/plain");
        let first = select(
            &resource,
            &arguments(
                json!({"uri":resource.uri,"mode":"search","text":"match","context_lines":0}),
            ),
        )
        .unwrap()
        .structured_content
        .unwrap();
        assert_eq!(first["selection"]["data"], "match\n");
        let second = select(&resource,&arguments(json!({"uri":resource.uri,"mode":"search","text":"match","context_lines":1,"offset":first["selection"]["next_offset"]}))).unwrap().structured_content.unwrap();
        assert_eq!(second["selection"]["data"], "three\nmatch\nfive\n");
    }

    #[test]
    fn unicode_ranges_resume_without_gaps_and_invalid_offsets_fail() {
        let resource = resource("💥alpha\n".repeat(100).into_bytes(), "text/plain");
        let mut offset = 0;
        let mut recovered = String::new();
        loop {
            let result = select(
                &resource,
                &arguments(json!({"uri":resource.uri,"mode":"text","offset":offset,"limit":8})),
            )
            .unwrap();
            let selection = result.structured_content.unwrap()["selection"].clone();
            recovered.push_str(selection["data"].as_str().unwrap());
            let Some(next) = selection["next_offset"].as_u64() else {
                break;
            };
            assert!(next > offset);
            offset = next;
        }
        assert_eq!(recovered.as_bytes(), resource.body.as_ref());
        for offset in [1, resource.body.len() + 1] {
            assert!(
                select(
                    &resource,
                    &arguments(json!({"uri":resource.uri,"mode":"text","offset":offset}))
                )
                .is_err()
            );
        }
    }

    #[test]
    fn json_pointer_rows_and_fields_are_bounded_and_schema_valid() {
        let rows = (0..30)
            .map(|id| json!({"id":id,"name":format!("row-{id}"),"irrelevant":"x".repeat(1000)}))
            .collect::<Vec<_>>();
        let resource = resource(
            serde_json::to_vec(&json!({"rows":rows})).unwrap(),
            "application/json",
        );
        let result = select(&resource,&arguments(json!({"uri":resource.uri,"mode":"json","pointer":"/rows","offset":10,"limit":5,"fields":["id"]}))).unwrap();
        let value = result.structured_content.unwrap();
        assert_eq!(
            value["selection"]["data"],
            json!([{"id":10},{"id":11},{"id":12},{"id":13},{"id":14}])
        );
        assert_eq!(value["selection"]["next_offset"], 15);
        let schema = Value::Object((*tool().output_schema.unwrap()).clone());
        assert!(jsonschema::validator_for(&schema).unwrap().is_valid(&value));
    }
}
