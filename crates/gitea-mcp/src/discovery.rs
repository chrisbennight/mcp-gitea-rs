//! Catalog discovery tools: search over the registered surface, and a
//! description of one member of it.
//!
//! The catalog index resource hands a caller the whole operation catalog in
//! one read. These tools answer the narrower question — which operation does
//! X, and what exactly does it take — without the caller holding the index in
//! context. `catalog.search` filters and ranks the same rows the index
//! prints; `catalog.describe` expands one of them to its full contract. All
//! three describe the operation catalog itself, not the session's tool
//! listing: the operations are executed through the lanes rather than
//! published individually, so discovery is how a caller reaches one at all.
//! An operation the specification marks deprecated appears in no discovery
//! view, yet describe still answers for its exact name, because the operation
//! stays executable and a caller holding the name deserves its contract.
//!
//! Neither tool touches the upstream: both read the process-local registry, so
//! results are deterministic for a given build and carry no side effects.

use std::borrow::Cow;
use std::sync::Arc;

use gitea_api::catalog::{
    OperationRisk, OperationSpec, ParameterLocation, exposed_operation, exposed_operation_by_id,
    operation_catalog,
};
use rmcp::ErrorData as McpError;
use rmcp::model::{Meta, Tool, ToolAnnotations};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{IndexEntry, SENSITIVE_RESULT_META, json_object_schema, required_arguments};

pub const SEARCH_TOOL: &str = "catalog.search";
pub const DESCRIBE_TOOL: &str = "catalog.describe";

/// Most index rows one search reply carries, and the default when the caller
/// does not choose. A page is a selection aid, not a transfer format; a caller
/// that wants the whole surface reads the index resource instead.
const MAX_SEARCH_LIMIT: usize = 100;
const DEFAULT_SEARCH_LIMIT: usize = 25;

/// How many near-miss names an unknown `catalog.describe` argument reports.
const SUGGESTION_COUNT: usize = 3;

/// Character bounds on the free-text arguments, published as `maxLength` and
/// enforced before any normalization or matching. Every registered identifier
/// fits well inside them, so no legitimate call is refused — what they refuse
/// is a caller-sized string driving the lowercasing, substring, and
/// edit-distance work, which for the suggestion path is quadratic in the
/// input. The transport admits requests far larger than any identifier, so
/// the bound has to live here.
pub(crate) const MAX_NAME_CHARS: usize = 128;
const MAX_QUERY_CHARS: usize = 256;
const MAX_DOMAIN_CHARS: usize = 64;

/// Whether `value` exceeds `max` characters, without scanning an oversized
/// value: a UTF-8 character is at most four bytes, so the byte length alone
/// convicts anything past four times the bound.
pub(crate) fn exceeds_chars(value: &str, max: usize) -> bool {
    value.len() > max * 4 || value.chars().count() > max
}

pub fn tools() -> [Tool; 2] {
    [search_tool(), describe_tool()]
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchArguments {
    pub query: Option<String>,
    pub domain: Option<String>,
    pub risk: Option<String>,
    pub administrative: Option<bool>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DescribeArguments {
    pub name: String,
    pub detail: Option<DetailLevel>,
}

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DetailLevel {
    Summary,
    #[default]
    Full,
}

/// Filter and rank the index rows.
///
/// Every whitespace-separated query term must match somewhere — tool name,
/// operation id, domain, or summary, case-insensitively — so adding a term
/// narrows rather than widens. Ranking puts an exact name first, then name
/// matches, then everything that matched only through its summary; ties break
/// on the tool name so a repeated call pages through a stable order.
///
/// A result with no matches at all names the available domains instead of
/// returning a bare empty list, because the caller's next move is a different
/// query and the domain list is the cheapest way to reorient. An empty page
/// past the end of a nonzero match set instead names the page math, since the
/// query itself succeeded.
///
/// # Errors
///
/// Returns invalid-params for a filter value outside its published set or a
/// page bound outside its published range.
pub fn search(arguments: &SearchArguments, entries: &[IndexEntry]) -> Result<Value, McpError> {
    let limit = validated_limit(arguments)?;
    let offset = arguments.offset.unwrap_or(0);
    let domains = domain_names(entries);
    let domain = match arguments.domain.as_deref() {
        Some(domain) => {
            let normalized = domain.to_lowercase();
            if !domains.contains(&normalized) {
                return Err(McpError::invalid_params(
                    format!("unknown domain {domain}; domains: {}", domains.join(", ")),
                    None,
                ));
            }
            Some(normalized)
        }
        None => None,
    };

    let query = arguments
        .query
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let terms: Vec<&str> = query.split_whitespace().collect();

    let mut matches: Vec<(u8, &IndexEntry)> = entries
        .iter()
        .filter_map(|entry| {
            if let Some(domain) = &domain
                && entry.domain != *domain
            {
                return None;
            }
            if let Some(risk) = arguments.risk.as_deref()
                && entry.risk != risk
            {
                return None;
            }
            if let Some(administrative) = arguments.administrative
                && entry.administrative != administrative
            {
                return None;
            }
            let rank = rank(&query, &terms, entry)?;
            Some((rank, entry))
        })
        .collect();
    matches.sort_by(|left, right| (left.0, &left.1.tool).cmp(&(right.0, &right.1.tool)));

    let total = matches.len();
    let page: Vec<Value> = matches
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|(_, entry)| match_row(entry))
        .collect();

    let mut result = Map::from_iter([
        ("total".to_string(), json!(total)),
        ("offset".to_string(), json!(offset)),
        ("limit".to_string(), json!(limit)),
        ("matches".to_string(), Value::Array(page)),
    ]);
    if total == 0 {
        result.insert("domains".to_string(), json!(domains));
        result.insert(
            "hint".to_string(),
            json!(
                "no operation matched; use fewer or broader terms, drop filters, \
                 or read the catalog index resource"
            ),
        );
    } else if offset >= total {
        // Matches exist; the caller merely paged past them. The domain list
        // would be noise here — the useful reorientation is the page math.
        result.insert(
            "hint".to_string(),
            json!(format!(
                "offset {offset} is at or past the last match; {total} matched in total"
            )),
        );
    }
    Ok(Value::Object(result))
}

/// The page and filter arguments a search may proceed with: the page bound in
/// its published range, the risk value in its published set, and the free-text
/// arguments inside the character bounds — checked before any normalization or
/// matching touches them.
fn validated_limit(arguments: &SearchArguments) -> Result<usize, McpError> {
    let limit = arguments.limit.unwrap_or(DEFAULT_SEARCH_LIMIT);
    if limit == 0 || limit > MAX_SEARCH_LIMIT {
        return Err(McpError::invalid_params(
            format!("limit must be between 1 and {MAX_SEARCH_LIMIT}"),
            None,
        ));
    }
    if let Some(risk) = arguments.risk.as_deref()
        && !matches!(risk, "read" | "mutation" | "destructive")
    {
        return Err(McpError::invalid_params(
            "risk must be one of read, mutation, destructive",
            None,
        ));
    }
    if arguments
        .query
        .as_deref()
        .is_some_and(|query| exceeds_chars(query, MAX_QUERY_CHARS))
    {
        return Err(McpError::invalid_params(
            format!("query exceeds {MAX_QUERY_CHARS} characters"),
            None,
        ));
    }
    if arguments
        .domain
        .as_deref()
        .is_some_and(|domain| exceeds_chars(domain, MAX_DOMAIN_CHARS))
    {
        return Err(McpError::invalid_params(
            format!("domain exceeds {MAX_DOMAIN_CHARS} characters"),
            None,
        ));
    }
    Ok(limit)
}

/// One search match, in the index row's vocabulary plus the operation id.
fn match_row(entry: &IndexEntry) -> Value {
    let mut row = Map::from_iter([
        ("tool".to_string(), json!(entry.tool)),
        ("risk".to_string(), json!(entry.risk)),
        ("domain".to_string(), json!(entry.domain)),
        ("administrative".to_string(), json!(entry.administrative)),
        ("sensitive_result".to_string(), json!(entry.sensitive)),
        ("required".to_string(), json!(entry.required)),
        ("summary".to_string(), json!(entry.summary)),
    ]);
    if let Some(operation_id) = &entry.operation_id {
        row.insert("operation_id".to_string(), json!(operation_id));
    }
    Value::Object(row)
}

/// Describe one registered tool, by tool name or upstream operation id.
///
/// `full` returns the exact input schema the named tool validates against;
/// `summary` stops at the contract a caller needs to decide whether to load it.
/// An unknown name reports the nearest registered names, so a misremembered
/// identifier corrects itself in one round trip instead of a fresh search.
///
/// # Errors
///
/// Returns invalid-params for a name matching no registered tool or operation.
pub fn describe(arguments: &DescribeArguments, registered: &[Tool]) -> Result<Value, McpError> {
    if exceeds_chars(&arguments.name, MAX_NAME_CHARS) {
        // Refused before any normalization or matching: the suggestion path is
        // quadratic in this string, and nothing registered is anywhere near
        // this long.
        return Err(McpError::invalid_params(
            format!("name exceeds {MAX_NAME_CHARS} characters"),
            None,
        ));
    }
    let detail = arguments.detail.unwrap_or_default();
    let spec =
        exposed_operation(&arguments.name).or_else(|| exposed_operation_by_id(&arguments.name));
    if let Some(spec) = spec {
        return Ok(describe_generated(spec, detail));
    }
    // Hand-written replacements keep their upstream operation identity in the
    // published metadata channel, so an operation id whose generated entry is
    // unexposed still resolves to the tool that serves it.
    if let Some(tool) = registered.iter().find(|tool| {
        tool.name == arguments.name || meta_operation_id(tool) == Some(arguments.name.as_str())
    }) {
        return Ok(describe_hand_written(tool, detail));
    }
    // Suggestions span what describe can actually answer for: the published
    // tools and the catalog operations they execute. Drawing them from the
    // published list alone would offer eleven names for a mistyped operation.
    let mut candidates: Vec<&str> = registered.iter().map(|tool| tool.name.as_ref()).collect();
    candidates.extend(registered.iter().filter_map(meta_operation_id));
    candidates.extend(
        operation_catalog()
            .operations
            .iter()
            .filter(|operation| operation.exposed && !operation.deprecated.is_deprecated())
            .flat_map(|operation| {
                [
                    operation.tool_name.as_str(),
                    operation.operation_id.as_str(),
                ]
            }),
    );
    Err(McpError::invalid_params(
        format!(
            "no tool or operation named {}; nearest: {}",
            arguments.name,
            nearest_names(&arguments.name, &candidates).join(", ")
        ),
        None,
    ))
}

/// The upstream operation id a registered tool publishes, when it has one.
fn meta_operation_id(tool: &Tool) -> Option<&str> {
    tool.meta
        .as_ref()
        .and_then(|meta| meta.0.get("org.cacahuate/operationId"))
        .and_then(Value::as_str)
}

/// A boolean from a registered tool's published metadata channel; absent means
/// the tool has nothing to declare, which reads as false.
fn meta_bool(tool: &Tool, key: &str) -> bool {
    tool.meta
        .as_ref()
        .and_then(|meta| meta.0.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn describe_generated(spec: &OperationSpec, detail: DetailLevel) -> Value {
    let mut result = Map::from_iter([
        ("tool".to_string(), json!(spec.tool_name)),
        ("operation_id".to_string(), json!(spec.operation_id)),
        ("risk".to_string(), json!(risk_name(spec.risk))),
        (
            "domain".to_string(),
            json!(
                spec.tool_name
                    .split_once('.')
                    .map_or("server", |(domain, _)| domain)
            ),
        ),
        ("administrative".to_string(), json!(spec.administrative)),
        ("sensitive_result".to_string(), json!(spec.secret_result)),
        ("summary".to_string(), json!(spec.summary)),
        (
            "parameters".to_string(),
            Value::Array(
                spec.parameters
                    .iter()
                    .map(|parameter| {
                        json!({
                            "name": parameter.name,
                            "location": location_name(parameter.location),
                            "required": parameter.required,
                        })
                    })
                    .collect(),
            ),
        ),
    ]);
    if let Some(guidance) = &spec.agent_guidance {
        result.insert("guidance".to_string(), json!(guidance));
    }
    if detail == DetailLevel::Full {
        result.insert(
            "input_schema".to_string(),
            Value::Object(spec.input_schema.clone()),
        );
        if !spec.response_headers.is_empty() {
            result.insert("response_headers".to_string(), json!(spec.response_headers));
        }
    }
    Value::Object(result)
}

fn describe_hand_written(tool: &Tool, detail: DetailLevel) -> Value {
    let annotations = tool.annotations.as_ref();
    let risk = if annotations.and_then(|a| a.destructive_hint) == Some(true) {
        "destructive"
    } else if annotations.and_then(|a| a.read_only_hint) == Some(true) {
        "read"
    } else {
        "mutation"
    };
    let mut result = Map::from_iter([
        ("tool".to_string(), json!(tool.name)),
        ("risk".to_string(), json!(risk)),
        (
            "domain".to_string(),
            json!(
                tool.name
                    .split_once('.')
                    .map_or("server", |(domain, _)| domain)
            ),
        ),
        (
            "administrative".to_string(),
            json!(meta_bool(tool, "org.cacahuate/administrative")),
        ),
        (
            "sensitive_result".to_string(),
            json!(meta_bool(tool, SENSITIVE_RESULT_META)),
        ),
        (
            "summary".to_string(),
            json!(tool.description.as_deref().unwrap_or_default()),
        ),
        (
            "required".to_string(),
            json!(required_arguments(&tool.input_schema)),
        ),
    ]);
    if let Some(operation_id) = meta_operation_id(tool) {
        result.insert("operation_id".to_string(), json!(operation_id));
    }
    if detail == DetailLevel::Full {
        result.insert(
            "input_schema".to_string(),
            Value::Object(tool.input_schema.as_ref().clone()),
        );
    }
    Value::Object(result)
}

/// Rank a filtered row against the query, or reject it.
///
/// `None` means a term matched nowhere. An empty query matches everything at
/// the weakest rank, so bare filters still list their whole selection.
fn rank(query: &str, terms: &[&str], entry: &IndexEntry) -> Option<u8> {
    if terms.is_empty() {
        return Some(3);
    }
    let tool = entry.tool.to_lowercase();
    let operation_id = entry.operation_id.as_deref().map(str::to_lowercase);
    let summary = entry.summary.to_lowercase();
    for term in terms {
        let in_name = tool.contains(term)
            || operation_id.as_deref().is_some_and(|id| id.contains(term))
            || entry.domain.contains(term);
        if !in_name && !summary.contains(term) {
            return None;
        }
    }
    if tool == query || operation_id.as_deref() == Some(query) {
        return Some(0);
    }
    if tool.contains(query) {
        return Some(1);
    }
    let all_terms_in_name = terms.iter().all(|term| {
        tool.contains(term)
            || operation_id.as_deref().is_some_and(|id| id.contains(term))
            || entry.domain.contains(term)
    });
    Some(if all_terms_in_name { 2 } else { 3 })
}

fn domain_names(entries: &[IndexEntry]) -> Vec<String> {
    let mut domains: Vec<String> = entries.iter().map(|entry| entry.domain.clone()).collect();
    domains.sort();
    domains.dedup();
    domains
}

fn risk_name(risk: OperationRisk) -> &'static str {
    match risk {
        OperationRisk::Read => "read",
        OperationRisk::Mutation => "mutation",
        OperationRisk::Destructive => "destructive",
    }
}

fn location_name(location: ParameterLocation) -> &'static str {
    match location {
        ParameterLocation::Path => "path",
        ParameterLocation::Query => "query",
        ParameterLocation::Header => "header",
        ParameterLocation::Body => "body",
        ParameterLocation::FormData => "formData",
    }
}

/// The closest registered names to a miss, nearest first, ties alphabetical.
pub(crate) fn nearest_names(name: &str, candidates: &[&str]) -> Vec<String> {
    let mut scored: Vec<(usize, &str)> = candidates
        .iter()
        .map(|candidate| {
            (
                edit_distance(&name.to_lowercase(), &candidate.to_lowercase()),
                *candidate,
            )
        })
        .collect();
    scored.sort_by(|left, right| (left.0, left.1).cmp(&(right.0, right.1)));
    scored
        .into_iter()
        .take(SUGGESTION_COUNT)
        .map(|(_, candidate)| candidate.to_string())
        .collect()
}

/// Levenshtein distance over characters. Names are short and the candidate set
/// is the registry, so the quadratic table stays trivially small.
fn edit_distance(left: &str, right: &str) -> usize {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0; right.len() + 1];
    for (row, left_char) in left.iter().enumerate() {
        current[0] = row + 1;
        for (column, right_char) in right.iter().enumerate() {
            let substitution = previous[column] + usize::from(left_char != right_char);
            current[column + 1] = substitution
                .min(previous[column + 1] + 1)
                .min(current[column] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

/// The exact-risk channel for a discovery tool: read risk, not
/// administrative, no sensitive result, and no upstream operation id to name.
/// Published even though every value is the quiet default, so a caller that
/// classifies the surface through this channel sees one uniform vocabulary
/// instead of a gap on exactly these tools.
fn discovery_meta() -> Meta {
    Meta(Map::from_iter([
        (
            "org.cacahuate/risk".to_string(),
            Value::String("read".to_string()),
        ),
        (
            "org.cacahuate/administrative".to_string(),
            Value::Bool(false),
        ),
        (SENSITIVE_RESULT_META.to_string(), Value::Bool(false)),
    ]))
}

fn search_tool() -> Tool {
    let mut tool = Tool::new(
        Cow::Borrowed(SEARCH_TOOL),
        Cow::Borrowed(
            "Search the registered tool catalog by keyword, domain, risk, or administrative \
             flag. Every query term must match; results are index rows naming the tool, its \
             risk, and its required arguments. Follow up with catalog.describe for one tool's \
             schema. Read-only, local to the server.",
        ),
        Arc::new(json_object_schema(
            json!({
                "query": {
                    "type": "string",
                    "maxLength": MAX_QUERY_CHARS,
                    "description": "whitespace-separated terms matched against tool name, \
                                    operation id, domain, and summary"
                },
                "domain": {
                    "type": "string",
                    "maxLength": MAX_DOMAIN_CHARS,
                    "description": "exact domain, as in the index"
                },
                "risk": {"type": "string", "enum": ["read", "mutation", "destructive"]},
                "administrative": {"type": "boolean"},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_SEARCH_LIMIT},
                "offset": {"type": "integer", "minimum": 0}
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
    tool.output_schema = Some(Arc::new(crate::displaceable_output_schema(
        json!({
            "total": {"type": "integer"},
            "offset": {"type": "integer"},
            "limit": {"type": "integer"},
            "matches": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "tool": {"type": "string"},
                        "operation_id": {"type": "string"},
                        "risk": {"type": "string"},
                        "domain": {"type": "string"},
                        "administrative": {"type": "boolean"},
                        "sensitive_result": {"type": "boolean"},
                        "required": {"type": "string"},
                        "summary": {"type": "string"}
                    },
                    "required": [
                        "tool", "risk", "domain", "administrative", "sensitive_result",
                        "required", "summary"
                    ],
                    "additionalProperties": false
                }
            },
            "domains": {"type": "array", "items": {"type": "string"}},
            "hint": {"type": "string"}
        }),
        &["total", "offset", "limit", "matches"],
    )));
    tool.meta = Some(discovery_meta());
    tool
}

fn describe_tool() -> Tool {
    let mut tool = Tool::new(
        Cow::Borrowed(DESCRIBE_TOOL),
        Cow::Borrowed(
            "Describe one callable operation by tool name or upstream operation id, \
             including deprecated operations that no listing carries but that remain \
             callable by exact name. Detail `summary` gives the contract without the \
             schema; `full` (the default) includes the exact input schema the operation \
             validates against. An unknown name returns the nearest registered names. \
             Read-only, local to the server.",
        ),
        Arc::new(json_object_schema(
            json!({
                "name": {
                    "type": "string",
                    "maxLength": MAX_NAME_CHARS,
                    "description": "tool name such as repository.get, or upstream operation \
                                    id such as repoGet"
                },
                "detail": {"type": "string", "enum": ["summary", "full"]}
            }),
            &["name"],
        )),
    )
    .with_annotations(
        ToolAnnotations::new()
            .read_only(true)
            .destructive(false)
            .idempotent(true)
            .open_world(false),
    );
    tool.output_schema = Some(Arc::new(crate::displaceable_output_schema(
        json!({
            "tool": {"type": "string"},
            "operation_id": {"type": "string"},
            "risk": {"type": "string"},
            "domain": {"type": "string"},
            "administrative": {"type": "boolean"},
            "sensitive_result": {"type": "boolean"},
            "summary": {"type": "string"},
            "guidance": {"type": "string"},
            "required": {"type": "string"},
            "parameters": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "location": {"type": "string"},
                        "required": {"type": "boolean"}
                    },
                    "required": ["name", "location", "required"],
                    "additionalProperties": false
                }
            },
            "input_schema": {"type": "object"},
            "response_headers": {"type": "array", "items": {"type": "string"}}
        }),
        &["tool", "risk", "domain"],
    )));
    tool.meta = Some(discovery_meta());
    tool
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_entries;

    fn search_value(arguments: &SearchArguments) -> Value {
        search(arguments, &index_entries()).expect("valid search")
    }

    fn matches(value: &Value) -> &Vec<Value> {
        value["matches"].as_array().expect("matches array")
    }

    #[test]
    fn exact_tool_name_ranks_first() {
        let value = search_value(&SearchArguments {
            query: Some("repository.get".to_string()),
            ..SearchArguments::default()
        });
        assert_eq!(matches(&value)[0]["tool"], json!("repository.get"));
    }

    #[test]
    fn exact_operation_id_ranks_its_tool_first() {
        let value = search_value(&SearchArguments {
            query: Some("repoGet".to_string()),
            ..SearchArguments::default()
        });
        assert_eq!(matches(&value)[0]["tool"], json!("repository.get"));
        assert_eq!(matches(&value)[0]["operation_id"], json!("repoGet"));
    }

    #[test]
    fn every_term_must_match_somewhere() {
        let broad = search_value(&SearchArguments {
            query: Some("merge".to_string()),
            ..SearchArguments::default()
        });
        let narrow = search_value(&SearchArguments {
            query: Some("merge pull".to_string()),
            ..SearchArguments::default()
        });
        assert!(narrow["total"].as_u64() <= broad["total"].as_u64());
        assert!(
            matches(&narrow)
                .iter()
                .any(|row| row["tool"] == json!("repository.merge_pull_request"))
        );
    }

    #[test]
    fn filters_constrain_domain_and_risk() {
        let value = search_value(&SearchArguments {
            domain: Some("issue".to_string()),
            risk: Some("destructive".to_string()),
            limit: Some(100),
            ..SearchArguments::default()
        });
        assert!(value["total"].as_u64().expect("total") > 0);
        for row in matches(&value) {
            assert_eq!(row["domain"], json!("issue"));
            assert_eq!(row["risk"], json!("destructive"));
        }
    }

    #[test]
    fn administrative_filter_selects_admin_operations() {
        let value = search_value(&SearchArguments {
            administrative: Some(true),
            limit: Some(100),
            ..SearchArguments::default()
        });
        assert!(value["total"].as_u64().expect("total") > 0);
        for row in matches(&value) {
            assert_eq!(row["administrative"], json!(true));
        }
    }

    #[test]
    fn empty_result_names_domains_and_a_next_step() {
        let value = search_value(&SearchArguments {
            query: Some("nothing-matches-this-term".to_string()),
            ..SearchArguments::default()
        });
        assert_eq!(value["total"], json!(0));
        assert!(
            value["domains"]
                .as_array()
                .expect("domains")
                .contains(&json!("repository"))
        );
        assert!(value["hint"].as_str().expect("hint").contains("broader"));
    }

    #[test]
    fn out_of_range_limit_is_rejected() {
        let entries = index_entries();
        for limit in [0, MAX_SEARCH_LIMIT + 1] {
            let error = search(
                &SearchArguments {
                    limit: Some(limit),
                    ..SearchArguments::default()
                },
                &entries,
            )
            .expect_err("limit outside the published range");
            assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        }
    }

    #[test]
    fn unknown_domain_and_risk_are_rejected_with_the_valid_sets() {
        let entries = index_entries();
        let error = search(
            &SearchArguments {
                domain: Some("gitlab".to_string()),
                ..SearchArguments::default()
            },
            &entries,
        )
        .expect_err("unknown domain");
        assert!(error.message.contains("repository"));
        let error = search(
            &SearchArguments {
                risk: Some("harmless".to_string()),
                ..SearchArguments::default()
            },
            &entries,
        )
        .expect_err("unknown risk");
        assert!(error.message.contains("read, mutation, destructive"));
    }

    #[test]
    fn pages_are_disjoint_stable_and_cover_the_same_order() {
        let both = search_value(&SearchArguments {
            domain: Some("repository".to_string()),
            limit: Some(20),
            ..SearchArguments::default()
        });
        let first = search_value(&SearchArguments {
            domain: Some("repository".to_string()),
            limit: Some(10),
            ..SearchArguments::default()
        });
        let second = search_value(&SearchArguments {
            domain: Some("repository".to_string()),
            limit: Some(10),
            offset: Some(10),
            ..SearchArguments::default()
        });
        let first_again = search_value(&SearchArguments {
            domain: Some("repository".to_string()),
            limit: Some(10),
            ..SearchArguments::default()
        });
        assert_eq!(first, first_again);
        assert_eq!(matches(&first).len(), 10);
        assert_eq!(matches(&second).len(), 10);
        let paged: Vec<&Value> = matches(&first)
            .iter()
            .chain(matches(&second).iter())
            .collect();
        let unpaged: Vec<&Value> = matches(&both).iter().collect();
        assert_eq!(paged, unpaged);
    }

    #[test]
    fn deprecated_operations_are_not_searchable_but_describe_by_name() {
        let value = search_value(&SearchArguments {
            query: Some("delete_comment_deprecated".to_string()),
            ..SearchArguments::default()
        });
        assert_eq!(value["total"], json!(0));
        let described = describe(
            &DescribeArguments {
                name: "issue.delete_comment_deprecated".to_string(),
                detail: Some(DetailLevel::Summary),
            },
            &crate::GiteaMcp::list_tools_payload().tools,
        )
        .expect("exact name still answers for a dispatchable operation");
        assert_eq!(described["tool"], json!("issue.delete_comment_deprecated"));
    }

    #[test]
    fn an_overrun_offset_names_the_page_math() {
        let value = search_value(&SearchArguments {
            query: Some("repository.get".to_string()),
            offset: Some(10_000),
            ..SearchArguments::default()
        });
        assert!(value["total"].as_u64().expect("total") > 0);
        assert!(matches(&value).is_empty());
        assert!(value.get("domains").is_none());
        assert!(
            value["hint"]
                .as_str()
                .expect("hint")
                .contains("past the last match")
        );
    }

    #[test]
    fn describe_full_returns_the_exact_generated_schema() {
        let value = describe(
            &DescribeArguments {
                name: "repository.get".to_string(),
                detail: None,
            },
            &[],
        )
        .expect("known tool");
        let spec = exposed_operation("repository.get").expect("spec");
        assert_eq!(
            value["input_schema"],
            Value::Object(spec.input_schema.clone())
        );
        assert_eq!(value["operation_id"], json!("repoGet"));
        assert_eq!(value["risk"], json!("read"));
    }

    #[test]
    fn describe_summary_omits_the_schema_but_keeps_the_contract() {
        let value = describe(
            &DescribeArguments {
                name: "repoGet".to_string(),
                detail: Some(DetailLevel::Summary),
            },
            &[],
        )
        .expect("known operation id");
        assert_eq!(value["tool"], json!("repository.get"));
        assert!(value.get("input_schema").is_none());
        assert!(
            value["parameters"]
                .as_array()
                .is_some_and(|p| !p.is_empty())
        );
    }

    #[test]
    fn describe_covers_hand_written_tools() {
        let registered = crate::GiteaMcp::list_tools_payload().tools;
        let value = describe(
            &DescribeArguments {
                name: "access_token.create".to_string(),
                detail: None,
            },
            &registered,
        )
        .expect("hand-written tool");
        assert_eq!(value["risk"], json!("mutation"));
        assert_eq!(value["required"], json!("name,scopes"));
        assert!(value["input_schema"].is_object());
        assert_eq!(value["operation_id"], json!("userCreateToken"));
        assert_eq!(value["administrative"], json!(true));
        assert_eq!(value["sensitive_result"], json!(true));
    }

    #[test]
    fn describe_resolves_hand_written_operation_ids() {
        let registered = crate::GiteaMcp::list_tools_payload().tools;
        for (operation_id, tool) in [
            ("userCreateToken", "access_token.create"),
            ("userGetTokens", "access_token.list"),
            ("userDeleteAccessToken", "access_token.revoke"),
            ("repositoryBootstrap", "repository.bootstrap"),
            ("getVersion", "server.version"),
        ] {
            let value = describe(
                &DescribeArguments {
                    name: operation_id.to_string(),
                    detail: Some(DetailLevel::Summary),
                },
                &registered,
            )
            .expect("operation id resolves to its replacement tool");
            assert_eq!(value["tool"], json!(tool));
            assert_eq!(value["operation_id"], json!(operation_id));
        }
    }

    #[test]
    fn administrative_filter_covers_hand_written_tools() {
        let value = search_value(&SearchArguments {
            administrative: Some(true),
            limit: Some(100),
            ..SearchArguments::default()
        });
        let tools: Vec<&Value> = matches(&value).iter().map(|row| &row["tool"]).collect();
        assert!(tools.contains(&&json!("access_token.create")));
        assert!(tools.contains(&&json!("repository.bootstrap")));
        let negated = search_value(&SearchArguments {
            query: Some("access_token".to_string()),
            administrative: Some(false),
            ..SearchArguments::default()
        });
        for row in matches(&negated) {
            assert_ne!(row["tool"], json!("access_token.create"));
        }
    }

    #[test]
    fn search_rows_carry_hand_written_operation_ids() {
        let value = search_value(&SearchArguments {
            query: Some("access_token.list".to_string()),
            ..SearchArguments::default()
        });
        assert_eq!(matches(&value)[0]["operation_id"], json!("userGetTokens"));
    }

    #[test]
    fn oversized_free_text_arguments_are_refused_before_matching() {
        let registered: Vec<Tool> = Vec::new();
        let error = describe(
            &DescribeArguments {
                name: "x".repeat(MAX_NAME_CHARS * 64),
                detail: None,
            },
            &registered,
        )
        .expect_err("oversized name");
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(error.message.contains("exceeds"));

        let entries = index_entries();
        let error = search(
            &SearchArguments {
                query: Some("q".repeat(MAX_QUERY_CHARS * 64)),
                ..SearchArguments::default()
            },
            &entries,
        )
        .expect_err("oversized query");
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        let error = search(
            &SearchArguments {
                domain: Some("d".repeat(MAX_DOMAIN_CHARS * 64)),
                ..SearchArguments::default()
            },
            &entries,
        )
        .expect_err("oversized domain");
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn describe_unknown_name_reports_nearest_names() {
        let registered = crate::GiteaMcp::list_tools_payload().tools;
        let error = describe(
            &DescribeArguments {
                name: "repository.gte".to_string(),
                detail: None,
            },
            &registered,
        )
        .expect_err("unknown name");
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(error.message.contains("repository.get"));
    }

    #[test]
    fn discovery_tools_are_read_only_and_closed() {
        for tool in tools() {
            let annotations = tool.annotations.as_ref().expect("annotations");
            assert_eq!(annotations.read_only_hint, Some(true));
            assert_eq!(annotations.destructive_hint, Some(false));
            assert_eq!(
                tool.input_schema.get("additionalProperties"),
                Some(&Value::Bool(false))
            );
            let meta = tool.meta.as_ref().expect("metadata channel");
            assert_eq!(meta.0.get("org.cacahuate/risk"), Some(&json!("read")));
            assert_eq!(
                meta.0.get("org.cacahuate/administrative"),
                Some(&json!(false))
            );
            assert_eq!(meta.0.get(SENSITIVE_RESULT_META), Some(&json!(false)));
        }
    }
}
