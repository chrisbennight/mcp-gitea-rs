use std::str::FromStr;

use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};
use bytes::Bytes;
use reqwest::{
    Method, RequestBuilder,
    header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue},
    multipart::{Form, Part},
};
use serde::Serialize;
use serde_json::{Map, Value, json};
use url::Url;
use zeroize::{Zeroize, Zeroizing};

use crate::{
    ApiError, GiteaClient,
    catalog::{AuthLane, OperationSpec, ParameterLocation, ParameterSpec},
};

const MAX_OPERATION_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_ERROR_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_DECLARED_RESPONSE_HEADER_BYTES: usize = 64 * 1024;
const MAX_UPLOAD_BYTES: usize = 48 * 1024 * 1024;

#[derive(Serialize, PartialEq)]
pub struct OperationResponse {
    pub operation_id: String,
    pub status: u16,
    pub success: bool,
    pub content_type: Option<String>,
    pub headers: Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pagination: Option<crate::Pagination>,
    pub data: Value,
}

impl GiteaClient {
    /// Execute one generated operation with already schema-validated arguments.
    ///
    /// # Errors
    ///
    /// Returns a normalized error when request construction, transport, or
    /// bounded response decoding fails. HTTP error responses remain structured
    /// operation results so callers can inspect their status and safe body.
    pub async fn execute_operation(
        &self,
        operation: &OperationSpec,
        arguments: Map<String, Value>,
    ) -> Result<OperationResponse, ApiError> {
        self.execute_operation_inner(operation, arguments, None)
            .await
    }

    /// Execute the repository-secret operation without materializing its value
    /// in the general JSON argument tree.
    ///
    /// # Errors
    ///
    /// Returns an error for any other operation or when construction,
    /// transport, or bounded response handling fails.
    pub async fn execute_repository_secret(
        &self,
        operation: &OperationSpec,
        arguments: Map<String, Value>,
        secret: Zeroizing<String>,
    ) -> Result<OperationResponse, ApiError> {
        if operation.operation_id != "updateRepoSecret" {
            return Err(ApiError::InvalidOperation);
        }
        self.execute_operation_inner(operation, arguments, Some(secret))
            .await
    }

    async fn execute_operation_inner(
        &self,
        operation: &OperationSpec,
        arguments: Map<String, Value>,
        secret: Option<Zeroizing<String>>,
    ) -> Result<OperationResponse, ApiError> {
        if !operation.exposed || operation.auth_lane != AuthLane::ServicePat {
            return Err(ApiError::UnsupportedOperation);
        }
        let method = Method::from_bytes(operation.method.as_bytes())
            .map_err(|_| ApiError::InvalidOperation)?;
        let url = operation_url(&self.base_url, operation, &arguments)?;
        let mut request = self.http.request(method, url);
        request = apply_query(request, operation, &arguments)?;
        request = apply_headers(request, operation, &arguments)?;
        request = match secret.as_deref() {
            Some(value) => apply_repository_secret_payload(request, value)?,
            None => apply_payload(request, operation, &arguments)?,
        };

        let mut response = request.send().await.map_err(ApiError::Transport)?;
        let status = response.status();
        // Gitea has answered, so the operation's outcome is settled whatever
        // happens below. Every failure from here on carries the status, because
        // an error that hides it reads like a request that never ran — and the
        // obvious response to that is to send it again.
        let delivered = |error: ApiError| error.delivered_with(status.as_u16());
        let response_bound = if status.is_success() {
            MAX_OPERATION_RESPONSE_BYTES
        } else {
            MAX_ERROR_RESPONSE_BYTES
        };
        // A refusal whose body cannot be read is still a refusal, and the status
        // is what a caller acts on. Returning an error there would hide a
        // delivered answer behind something that reads like the request never
        // ran. A *successful* response that cannot be read stays an error,
        // because there the body was the whole point of the call.
        let refused = || OperationResponse {
            operation_id: operation.operation_id.clone(),
            status: status.as_u16(),
            success: false,
            content_type: None,
            headers: Map::new(),
            pagination: None,
            data: json!({"message": "Gitea rejected the operation"}),
        };
        if response
            .content_length()
            .is_some_and(|length| length > response_bound as u64)
        {
            if !status.is_success() {
                return Ok(refused());
            }
            return Err(delivered(ApiError::ResponseTooLarge));
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let headers = match declared_response_headers(operation, response.headers()) {
            Ok(headers) => headers,
            // A refusal's declared headers are not what a caller came for, so an
            // unreadable one costs them the headers and nothing else. Returning
            // here instead would also discard the body, which is where the
            // reason lives — trading one loss for a larger one.
            Err(_) if !status.is_success() => Map::new(),
            Err(error) => return Err(delivered(error)),
        };
        let mut pagination = pagination_for(operation, &response);
        let mut body = Vec::new();
        loop {
            match response.chunk().await {
                Ok(None) => break,
                Ok(Some(chunk)) => {
                    if body.len().saturating_add(chunk.len()) > response_bound {
                        if !status.is_success() {
                            return Ok(refused());
                        }
                        return Err(delivered(ApiError::ResponseTooLarge));
                    }
                    body.extend_from_slice(&chunk);
                }
                Err(error) => {
                    if !status.is_success() {
                        return Ok(refused());
                    }
                    return Err(delivered(ApiError::InvalidResponse(error)));
                }
            }
        }
        let decoding_content_type = content_type
            .as_deref()
            .or_else(|| operation.produces.as_slice().first().map(String::as_str));
        let data = if secret.is_some() {
            if status.is_success() {
                Value::Null
            } else {
                json!({"message": crate::GENERIC_REFUSAL})
            }
        } else if status.is_success() {
            decode_response_body(decoding_content_type, &body).map_err(delivered)?
        } else {
            normalize_error_data(decoding_content_type, &body, &arguments, &self.secrets())
        };
        if let Some(pagination) = &mut pagination {
            pagination.observe_empty_page(data.as_array().is_some_and(Vec::is_empty));
        }
        Ok(OperationResponse {
            operation_id: operation.operation_id.clone(),
            status: status.as_u16(),
            success: status.is_success(),
            content_type,
            headers,
            pagination,
            data,
        })
    }
}

fn pagination_for(
    operation: &OperationSpec,
    response: &reqwest::Response,
) -> Option<crate::Pagination> {
    (response.status().is_success()
        && operation.parameters.iter().any(|parameter| {
            matches!(parameter.location, ParameterLocation::Query)
                && matches!(parameter.name.as_str(), "page" | "limit")
        }))
    .then(|| crate::Pagination::from_headers(response.headers(), response.url()))
}

#[derive(Serialize)]
struct RepositorySecretBody<'a> {
    data: &'a str,
}

struct SecretRequestBytes(Vec<u8>);

impl AsRef<[u8]> for SecretRequestBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for SecretRequestBytes {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

fn apply_repository_secret_payload(
    request: RequestBuilder,
    secret: &str,
) -> Result<RequestBuilder, ApiError> {
    // JSON can expand each input byte to a six-byte Unicode escape. Reserving
    // that worst case plus the fixed object syntax prevents a secret-bearing
    // allocation from being abandoned during serialization.
    let capacity = secret
        .len()
        .checked_mul(6)
        .and_then(|size| size.checked_add(16))
        .ok_or(ApiError::InvalidOperation)?;
    let mut encoded = Zeroizing::new(Vec::with_capacity(capacity));
    serde_json::to_writer(&mut *encoded, &RepositorySecretBody { data: secret })
        .map_err(|_| ApiError::InvalidOperation)?;
    debug_assert!(encoded.len() <= capacity);
    let owned = SecretRequestBytes(std::mem::take(&mut *encoded));
    let body = reqwest::Body::from(Bytes::from_owner(owned));
    Ok(request.header(CONTENT_TYPE, "application/json").body(body))
}

fn operation_url(
    base_url: &Url,
    operation: &OperationSpec,
    arguments: &Map<String, Value>,
) -> Result<Url, ApiError> {
    let mut url = base_url.clone();
    let mut segments = url
        .path_segments_mut()
        .map_err(|()| ApiError::InvalidOperation)?;
    segments.pop_if_empty();
    for template_segment in operation.path.trim_start_matches('/').split('/') {
        segments.push(&render_path_segment(template_segment, arguments)?);
    }
    drop(segments);
    Ok(url)
}

fn render_path_segment(template: &str, arguments: &Map<String, Value>) -> Result<String, ApiError> {
    let mut rendered = String::new();
    let mut remaining = template;
    while let Some(open) = remaining.find('{') {
        rendered.push_str(&remaining[..open]);
        let after_open = &remaining[open + 1..];
        let close = after_open.find('}').ok_or(ApiError::InvalidOperation)?;
        let name = &after_open[..close];
        if name.is_empty() || name.contains('{') {
            return Err(ApiError::InvalidOperation);
        }
        let value = arguments
            .get(name)
            .ok_or_else(|| ApiError::InvalidArguments(name.to_string()))?;
        let value = scalar_string(value)?;
        validate_path_segment(name, &value)?;
        rendered.push_str(&value);
        remaining = &after_open[close + 1..];
    }
    if remaining.contains('}') {
        return Err(ApiError::InvalidOperation);
    }
    rendered.push_str(remaining);
    Ok(rendered)
}

/// Reject empty or URL-normalizing dot path values.
///
/// # Errors
///
/// Returns [`ApiError::InvalidArguments`] for invalid segments.
pub fn validate_path_segment(name: &str, value: &str) -> Result<(), ApiError> {
    if value.is_empty() || matches!(value, "." | "..") {
        return Err(ApiError::InvalidArguments(name.to_string()));
    }
    Ok(())
}

fn apply_query(
    request: RequestBuilder,
    operation: &OperationSpec,
    arguments: &Map<String, Value>,
) -> Result<RequestBuilder, ApiError> {
    let mut pairs = Vec::new();
    for parameter in parameters_at(operation, ParameterLocation::Query) {
        let Some(value) = arguments.get(&parameter.name) else {
            continue;
        };
        append_parameter_values(&mut pairs, parameter, value)?;
    }
    Ok(request.query(&pairs))
}

fn append_parameter_values(
    pairs: &mut Vec<(String, String)>,
    parameter: &ParameterSpec,
    value: &Value,
) -> Result<(), ApiError> {
    if let Value::Array(values) = value {
        let rendered: Result<Vec<_>, _> = values.iter().map(scalar_string).collect();
        let rendered = rendered?;
        if parameter.collection_format.as_deref() == Some("multi") {
            pairs.extend(
                rendered
                    .into_iter()
                    .map(|value| (parameter.name.clone(), value)),
            );
        } else {
            let separator = match parameter.collection_format.as_deref() {
                Some("ssv") => " ",
                Some("tsv") => "\t",
                Some("pipes") => "|",
                _ => ",",
            };
            pairs.push((parameter.name.clone(), rendered.join(separator)));
        }
    } else {
        pairs.push((parameter.name.clone(), scalar_string(value)?));
    }
    Ok(())
}

fn apply_headers(
    mut request: RequestBuilder,
    operation: &OperationSpec,
    arguments: &Map<String, Value>,
) -> Result<RequestBuilder, ApiError> {
    for parameter in parameters_at(operation, ParameterLocation::Header) {
        let Some(value) = arguments.get(&parameter.name) else {
            continue;
        };
        let name = HeaderName::from_str(&parameter.name).map_err(|_| ApiError::InvalidHeader)?;
        if name == reqwest::header::AUTHORIZATION || name == reqwest::header::COOKIE {
            return Err(ApiError::InvalidHeader);
        }
        let value =
            HeaderValue::from_str(&scalar_string(value)?).map_err(|_| ApiError::InvalidHeader)?;
        request = request.header(name, value);
    }
    Ok(request)
}

fn apply_payload(
    mut request: RequestBuilder,
    operation: &OperationSpec,
    arguments: &Map<String, Value>,
) -> Result<RequestBuilder, ApiError> {
    let body_parameters: Vec<_> = parameters_at(operation, ParameterLocation::Body).collect();
    if body_parameters.len() > 1 {
        return Err(ApiError::InvalidOperation);
    }
    if let Some(parameter) = body_parameters.first()
        && let Some(value) = arguments.get(&parameter.name)
    {
        if operation
            .consumes
            .iter()
            .any(|content_type| content_type == "text/plain")
        {
            let text = value.as_str().ok_or_else(|| {
                ApiError::InvalidArguments(format!("{} must be a string", parameter.name))
            })?;
            request = request
                .header(CONTENT_TYPE, "text/plain")
                .body(text.to_string());
        } else {
            request = request.json(value);
        }
    }

    let form_parameters: Vec<_> = parameters_at(operation, ParameterLocation::FormData).collect();
    if !form_parameters.is_empty() {
        let mut form = Form::new();
        for parameter in form_parameters {
            let Some(value) = arguments.get(&parameter.name) else {
                continue;
            };
            if parameter.value_type.as_deref() == Some("file") {
                form = form.part(parameter.name.clone(), file_part(value)?);
            } else {
                form = form.text(parameter.name.clone(), scalar_string(value)?);
            }
        }
        request = request.multipart(form);
    }
    Ok(request)
}

fn file_part(value: &Value) -> Result<Part, ApiError> {
    let object = value.as_object().ok_or(ApiError::InvalidUpload)?;
    let filename = object
        .get("filename")
        .and_then(Value::as_str)
        .ok_or(ApiError::InvalidUpload)?;
    if filename.is_empty()
        || filename.len() > 255
        || filename
            .chars()
            .any(|character| matches!(character, '\r' | '\n' | '\0'))
    {
        return Err(ApiError::InvalidUpload);
    }
    let encoded = object
        .get("content_base64")
        .and_then(Value::as_str)
        .ok_or(ApiError::InvalidUpload)?;
    let bytes = STANDARD
        .decode(encoded)
        .or_else(|_| STANDARD_NO_PAD.decode(encoded))
        .map_err(|_| ApiError::InvalidUpload)?;
    if bytes.len() > MAX_UPLOAD_BYTES {
        return Err(ApiError::InvalidUpload);
    }
    let mut part = Part::bytes(bytes).file_name(filename.to_string());
    if let Some(media_type) = object.get("media_type").and_then(Value::as_str) {
        part = part
            .mime_str(media_type)
            .map_err(|_| ApiError::InvalidUpload)?;
    }
    Ok(part)
}

fn parameters_at(
    operation: &OperationSpec,
    location: ParameterLocation,
) -> impl Iterator<Item = &ParameterSpec> {
    operation
        .parameters
        .iter()
        .filter(move |parameter| parameter.location == location)
}

fn scalar_string(value: &Value) -> Result<String, ApiError> {
    match value {
        Value::String(value) => Ok(value.clone()),
        Value::Number(value) => Ok(value.to_string()),
        Value::Bool(value) => Ok(value.to_string()),
        _ => Err(ApiError::InvalidArguments(
            "expected scalar value".to_string(),
        )),
    }
}

fn decode_response_body(content_type: Option<&str>, body: &[u8]) -> Result<Value, ApiError> {
    if body.is_empty() {
        return Ok(Value::Null);
    }
    let normalized_content_type = content_type.map(str::to_ascii_lowercase);
    if normalized_content_type
        .as_deref()
        .is_some_and(|value| value.contains("application/json") || value.contains("+json"))
    {
        return serde_json::from_slice(body).map_err(ApiError::InvalidJson);
    }
    if normalized_content_type.as_deref().is_none_or(|value| {
        value.starts_with("text/") || value.contains("application/xml") || value.contains("+xml")
    }) && let Ok(text) = std::str::from_utf8(body)
    {
        return Ok(Value::String(text.to_string()));
    }
    Ok(json!({
        "encoding": "base64",
        "content": STANDARD.encode(body),
    }))
}

fn declared_response_headers(
    operation: &OperationSpec,
    headers: &HeaderMap,
) -> Result<Map<String, Value>, ApiError> {
    let mut result = Map::new();
    let mut total_bytes = 0_usize;
    for name in &operation.response_headers {
        let header_name = HeaderName::from_str(name).map_err(|_| ApiError::InvalidOperation)?;
        let mut values = Vec::new();
        for value in &headers.get_all(header_name) {
            let value = value
                .to_str()
                .map_err(|_| ApiError::InvalidResponseHeader)?;
            total_bytes = total_bytes.saturating_add(value.len());
            if total_bytes > MAX_DECLARED_RESPONSE_HEADER_BYTES {
                return Err(ApiError::ResponseTooLarge);
            }
            values.push(Value::String(value.to_string()));
        }
        match values.len() {
            0 => {}
            1 => {
                result.insert(name.clone(), values.pop().expect("one response header"));
            }
            _ => {
                result.insert(name.clone(), Value::Array(values));
            }
        }
    }
    Ok(result)
}

fn normalize_error_data(
    content_type: Option<&str>,
    body: &[u8],
    arguments: &Map<String, Value>,
    client_secrets: &[String],
) -> Value {
    // Sensitive argument values *and* the configured service token: the caller
    // can put a credential in the request, and the client always sends one. A
    // refusal body that echoes either must not carry it back out.
    let mut secrets = Vec::new();
    collect_sensitive_values(&Value::Object(arguments.clone()), None, &mut secrets);
    secrets.extend_from_slice(client_secrets);
    json!({"message": crate::reduce_error_message(content_type, body, &secrets)})
}

fn collect_sensitive_values(value: &Value, key: Option<&str>, found: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            for (name, nested) in object {
                collect_sensitive_values(nested, Some(name), found);
            }
        }
        Value::Array(values) => {
            for nested in values {
                collect_sensitive_values(nested, key, found);
            }
        }
        Value::String(value) if key.is_some_and(is_sensitive_name) && !value.is_empty() => {
            found.push(value.clone());
        }
        _ => {}
    }
}

fn is_sensitive_name(name: &str) -> bool {
    let normalized = name.to_ascii_lowercase().replace('-', "_");
    normalized.contains("password")
        || normalized.contains("secret")
        || normalized.contains("token")
        || normalized.contains("credential")
        || normalized.contains("authorization")
        || normalized == "cookie"
        || normalized.contains("private_key")
        || normalized == "content_base64"
        || normalized == "data"
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn binary_responses_are_explicitly_encoded() {
        assert_eq!(
            decode_response_body(Some("application/octet-stream"), &[0xff, 0x00])
                .expect("response"),
            json!({"encoding": "base64", "content": "/wA="})
        );
        assert_eq!(
            decode_response_body(Some("application/octet-stream"), b"hello").expect("response"),
            json!({"encoding": "base64", "content": "aGVsbG8="})
        );
    }

    #[test]
    fn path_parameters_cannot_normalize_out_of_the_api_root() {
        let operation =
            crate::catalog::exposed_operation("repository.get").expect("repository.get");
        let base = Url::parse("https://gitea.example.test/api/v1/").expect("URL");

        for value in ["", ".", ".."] {
            let arguments = Map::from_iter([
                ("owner".to_string(), Value::String(value.to_string())),
                ("repo".to_string(), Value::String("demo".to_string())),
            ]);
            assert!(matches!(
                operation_url(&base, operation, &arguments),
                Err(ApiError::InvalidArguments(_))
            ));
        }
    }

    #[test]
    fn upload_filenames_cannot_inject_multipart_headers() {
        let upload = json!({
            "filename": "report\r\nx-injected: true",
            "content_base64": "aGVsbG8="
        });

        assert!(matches!(file_part(&upload), Err(ApiError::InvalidUpload)));
    }

    #[tokio::test]
    async fn undeclared_pagination_headers_survive_normalization() {
        let (base_url, request) = loopback_response_with_headers(
            "200 OK",
            "application/json",
            br#"[{"number":1}]"#,
            None,
            "link: <?page=2&limit=100>; rel=\"next\"\r\nx-total-count: 125\r\n",
        )
        .await;
        let operation = crate::catalog::exposed_operation("repository.list_pull_requests").unwrap();
        let response = client(&base_url)
            .execute_operation(
                operation,
                Map::from_iter([
                    ("owner".into(), json!("acme")),
                    ("repo".into(), json!("widget")),
                    ("page".into(), json!(1)),
                    ("limit".into(), json!(100)),
                ]),
            )
            .await
            .unwrap();
        assert_eq!(
            response.pagination,
            Some(crate::Pagination {
                next_page: Some(2),
                total_count: Some(125),
                complete: Some(false),
            })
        );
        assert!(!response.headers.contains_key("link"));
        request.await.unwrap();
    }

    #[tokio::test]
    async fn query_parameters_are_encoded_from_the_typed_operation() {
        let (base_url, request) =
            loopback_response("200 OK", "application/json", b"[]", None).await;
        let operation =
            crate::catalog::exposed_operation("repository.list_pull_requests").expect("operation");
        let arguments = Map::from_iter([
            ("owner".to_string(), json!("alice smith")),
            ("repo".to_string(), json!("demo/odd")),
            ("base_branch".to_string(), json!("feature/one")),
            (
                "labels".to_string(),
                json!(["bug/security", "needs review"]),
            ),
        ]);

        let response = client(&base_url)
            .execute_operation(operation, arguments)
            .await
            .expect("operation response");

        assert!(response.success);
        let request =
            String::from_utf8(request.await.expect("request task")).expect("HTTP request");
        assert!(request.starts_with(
            "GET /api/v1/repos/alice%20smith/demo%2Fodd/pulls?base_branch=feature%2Fone&labels=bug%2Fsecurity&labels=needs+review HTTP/1.1\r\n"
        ));
        assert_service_token(&request);
    }

    #[tokio::test]
    async fn composite_path_placeholders_are_rendered_and_encoded() {
        let (base_url, request) =
            loopback_response("200 OK", "application/octet-stream", b"diff", None).await;
        let operation =
            crate::catalog::exposed_operation("repository.download_commit_diff_or_patch")
                .expect("operation");
        let arguments = Map::from_iter([
            ("owner".to_string(), json!("alice")),
            ("repo".to_string(), json!("demo")),
            ("sha".to_string(), json!("feature/one")),
            ("diffType".to_string(), json!("diff patch")),
        ]);

        let response = client(&base_url)
            .execute_operation(operation, arguments)
            .await
            .expect("operation response");

        assert!(response.success);
        let request =
            String::from_utf8(request.await.expect("request task")).expect("HTTP request");
        assert!(request.starts_with(
            "GET /api/v1/repos/alice/demo/git/commits/feature%2Fone.diff%20patch HTTP/1.1\r\n"
        ));
        assert_service_token(&request);
    }

    #[tokio::test]
    async fn preserving_redirects_are_returned_without_resubmitting_mutations() {
        let (base_url, request) = loopback_response_with_headers(
            "307 Temporary Redirect",
            "application/json",
            br#"{"message":"redirected"}"#,
            None,
            "location: /api/v1/redirected\r\n",
        )
        .await;
        let operation =
            crate::catalog::exposed_operation("repository.create_branch").expect("operation");
        let arguments = Map::from_iter([
            ("owner".to_string(), json!("alice")),
            ("repo".to_string(), json!("demo")),
            (
                "body".to_string(),
                json!({"new_branch_name": "feature", "old_branch_name": "main"}),
            ),
        ]);

        let response = client(&base_url)
            .execute_operation(operation, arguments)
            .await
            .expect("redirect remains structured");

        assert_eq!(response.status, 307);
        assert!(!response.success);
        assert_eq!(response.data, json!({"message": "redirected"}));
        request.await.expect("single upstream request");
    }

    #[tokio::test]
    async fn json_body_is_encoded_from_the_typed_operation() {
        let (base_url, request) = loopback_response(
            "201 Created",
            "application/json",
            br#"{"name":"feature"}"#,
            None,
        )
        .await;
        let operation =
            crate::catalog::exposed_operation("repository.create_branch").expect("operation");
        let arguments = Map::from_iter([
            ("owner".to_string(), json!("alice")),
            ("repo".to_string(), json!("demo")),
            (
                "body".to_string(),
                json!({"new_branch_name": "feature", "old_branch_name": "main"}),
            ),
        ]);

        let response = client(&base_url)
            .execute_operation(operation, arguments)
            .await
            .expect("operation response");

        assert_eq!(response.status, 201);
        let request =
            String::from_utf8(request.await.expect("request task")).expect("HTTP request");
        let (headers, body) = request.split_once("\r\n\r\n").expect("request body");
        assert!(headers.starts_with("POST /api/v1/repos/alice/demo/branches HTTP/1.1\r\n"));
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("content-type: application/json")
        );
        assert_service_token(headers);
        assert_eq!(
            serde_json::from_str::<Value>(body).expect("JSON body"),
            json!({"new_branch_name": "feature", "old_branch_name": "main"})
        );
    }

    #[tokio::test]
    async fn plain_text_body_uses_the_declared_content_type_without_json_quoting() {
        let (base_url, request) =
            loopback_response("200 OK", "text/html", b"<p>Hello</p>", None).await;
        let operation = crate::catalog::exposed_operation("miscellaneous.render_markdown_raw")
            .expect("operation");
        let arguments = Map::from_iter([("body".to_string(), json!("# Hello"))]);

        client(&base_url)
            .execute_operation(operation, arguments)
            .await
            .expect("operation response");

        let request =
            String::from_utf8(request.await.expect("request task")).expect("HTTP request");
        let (headers, body) = request.split_once("\r\n\r\n").expect("request body");
        assert!(headers.starts_with("POST /api/v1/markdown/raw HTTP/1.1\r\n"));
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("content-type: text/plain\r\n")
        );
        assert_eq!(body, "# Hello");
    }

    #[tokio::test]
    async fn multipart_upload_decodes_base64_and_keeps_query_metadata() {
        let (base_url, request) =
            loopback_response("201 Created", "application/json", br#"{"id":7}"#, None).await;
        let operation =
            crate::catalog::exposed_operation("issue.create_issue_attachment").expect("operation");
        let arguments = Map::from_iter([
            ("owner".to_string(), json!("alice")),
            ("repo".to_string(), json!("demo")),
            ("index".to_string(), json!(12)),
            ("name".to_string(), json!("report.txt")),
            (
                "attachment".to_string(),
                json!({
                    "filename": "report.txt",
                    "media_type": "text/plain",
                    "content_base64": "aGVsbG8="
                }),
            ),
        ]);

        client(&base_url)
            .execute_operation(operation, arguments)
            .await
            .expect("operation response");

        let request =
            String::from_utf8(request.await.expect("request task")).expect("HTTP request");
        let (headers, body) = request.split_once("\r\n\r\n").expect("request body");
        assert!(headers.starts_with(
            "POST /api/v1/repos/alice/demo/issues/12/assets?name=report.txt HTTP/1.1\r\n"
        ));
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("content-type: multipart/form-data; boundary=")
        );
        assert!(body.contains(r#"name="attachment"; filename="report.txt""#));
        assert!(body.contains("Content-Type: text/plain\r\n"));
        assert!(body.contains("\r\n\r\nhello\r\n"));
    }

    #[tokio::test]
    async fn upstream_http_errors_remain_structured_results() {
        let (base_url, request) = loopback_response(
            "404 Not Found",
            "application/json",
            br#"{"message":"not found"}"#,
            None,
        )
        .await;
        let operation = crate::catalog::exposed_operation("repository.get").expect("operation");
        let arguments = Map::from_iter([
            ("owner".to_string(), json!("alice")),
            ("repo".to_string(), json!("missing")),
        ]);

        let response = client(&base_url)
            .execute_operation(operation, arguments)
            .await
            .expect("structured HTTP response");

        assert_eq!(response.status, 404);
        assert!(!response.success);
        assert_eq!(response.data, json!({"message": "not found"}));
        request.await.expect("request task");
    }

    #[tokio::test]
    async fn declared_success_headers_are_returned_with_header_only_responses() {
        let (base_url, request) = loopback_response_with_headers(
            "200 OK",
            "application/json",
            b"",
            None,
            "token: runner-registration-secret\r\n",
        )
        .await;
        let operation = crate::catalog::exposed_operation("admin.create_runner_registration_token")
            .expect("operation");

        let response = client(&base_url)
            .execute_operation(operation, Map::new())
            .await
            .expect("operation response");

        assert_eq!(response.data, Value::Null);
        assert_eq!(
            response.headers,
            Map::from_iter([(
                "token".to_string(),
                Value::String("runner-registration-secret".to_string())
            )])
        );
        request.await.expect("request task");
    }

    #[tokio::test]
    async fn upstream_error_messages_redact_sensitive_argument_values() {
        let (base_url, request) = loopback_response(
            "422 Unprocessable Entity",
            "application/json",
            br#"{"message":"authorization value Bearer webhook-secret is invalid","detail":"not exposed"}"#,
            None,
        )
        .await;
        let operation =
            crate::catalog::exposed_operation("repository.create_hook").expect("operation");
        let arguments = Map::from_iter([
            ("owner".to_string(), json!("alice")),
            ("repo".to_string(), json!("demo")),
            (
                "body".to_string(),
                json!({
                    "type": "gitea",
                    "config": {"url": "https://hooks.example.test"},
                    "authorization_header": "Bearer webhook-secret"
                }),
            ),
        ]);

        let response = client(&base_url)
            .execute_operation(operation, arguments)
            .await
            .expect("structured HTTP response");

        assert_eq!(
            response.data,
            json!({"message": "authorization value [REDACTED] is invalid"})
        );
        request.await.expect("request task");
    }

    #[tokio::test]
    async fn declared_oversize_responses_fail_before_body_read() {
        let (base_url, request) = loopback_response(
            "200 OK",
            "application/octet-stream",
            b"",
            Some(MAX_OPERATION_RESPONSE_BYTES + 1),
        )
        .await;
        let operation =
            crate::catalog::exposed_operation("repository.get_archive").expect("archive operation");
        let arguments = Map::from_iter([
            ("owner".to_string(), json!("alice")),
            ("repo".to_string(), json!("demo")),
            ("archive".to_string(), json!("main.zip")),
        ]);

        let Err(error) = client(&base_url)
            .execute_operation(operation, arguments)
            .await
        else {
            panic!("a declared oversize response is refused")
        };
        assert!(
            matches!(
                &error,
                ApiError::ResponseNotDelivered { status: 200, source }
                    if matches!(**source, ApiError::ResponseTooLarge)
            ),
            "the status Gitea answered with survives the bound failure: {error:?}"
        );
        // The point of carrying the status: the caller learns the call ran.
        assert_eq!(
            error.call_outcome(),
            crate::CallOutcome::Completed { status: 200 }
        );
        request.await.expect("request task");
    }

    #[tokio::test]
    async fn a_mutation_whose_response_is_unreadable_reports_that_it_ran() {
        // The case that makes this contract worth having: a DELETE that Gitea
        // executed, whose reply cannot be delivered. Reported as a bare failure
        // it is indistinguishable from a request that never took effect, and the
        // natural response to that is to send it again.
        // 200 rather than the 204 a delete usually answers with: a 204 carries
        // no body by definition, so the transport discards its declared length
        // and there is nothing for the bound to reject.
        let (base_url, request) = loopback_response(
            "200 OK",
            "application/json",
            b"",
            Some(MAX_OPERATION_RESPONSE_BYTES + 1),
        )
        .await;
        let operation = crate::catalog::exposed_operation("repository.delete")
            .expect("repository delete operation");
        let arguments = Map::from_iter([
            ("owner".to_string(), json!("alice")),
            ("repo".to_string(), json!("demo")),
        ]);

        let Err(error) = client(&base_url)
            .execute_operation(operation, arguments)
            .await
        else {
            panic!("an undeliverable response is an error")
        };
        assert_eq!(
            error.call_outcome(),
            crate::CallOutcome::Completed { status: 200 },
            "the deletion happened and the caller must be able to tell: {error:?}"
        );
        request.await.expect("request task");
    }

    #[tokio::test]
    async fn a_refusal_with_an_unreadable_declared_header_is_still_a_refusal() {
        // Declared response headers are read before the refusal is normalized,
        // so an unreadable one used to cost the caller both the status and the
        // reason. They are not what a caller came for on a refusal.
        // A 0xFF byte in the value: hyper accepts it as obs-text, so it reaches
        // `declared_response_headers`, where `to_str` rejects it. A header hyper
        // itself refuses would fail earlier and prove nothing about this branch.
        let (base_url, request) = loopback_response_with_raw_header(
            "403 Forbidden",
            br#"{"message":"actions are disabled"}"#,
            b"token: bad\xffvalue\r\n",
        )
        .await;
        let operation = crate::catalog::exposed_operation("admin.create_runner_registration_token")
            .expect("runner token operation");

        let response = client(&base_url)
            .execute_operation(operation, Map::new())
            .await
            .expect("a refusal survives an unreadable declared header");
        assert_eq!(response.status, 403);
        assert!(!response.success);
        // The header is what was unreadable, so the header is what is lost. An
        // earlier version of this fix returned immediately and discarded the
        // body too, and this assertion is what would have caught it.
        assert_eq!(
            response.data["message"], "actions are disabled",
            "the readable reason survives an unreadable header"
        );
        assert!(
            response.headers.is_empty(),
            "the unreadable header is dropped: {:?}",
            response.headers
        );
        request.await.expect("request task");
    }

    #[tokio::test]
    async fn a_refusal_with_an_unreadable_body_is_still_a_refusal() {
        // The status is what a caller acts on. Turning an oversized refusal body
        // into an error would hide a delivered 409 behind something shaped like
        // a request that never ran.
        let (base_url, request) = loopback_response(
            "409 Conflict",
            "application/json",
            b"",
            Some(MAX_ERROR_RESPONSE_BYTES + 1),
        )
        .await;
        let operation =
            crate::catalog::exposed_operation("repository.get").expect("repository get operation");
        let arguments = Map::from_iter([
            ("owner".to_string(), json!("alice")),
            ("repo".to_string(), json!("demo")),
        ]);

        let response = client(&base_url)
            .execute_operation(operation, arguments)
            .await
            .expect("a refusal survives an unreadable body");
        assert_eq!(response.status, 409);
        assert!(!response.success);
        assert_eq!(
            response.data["message"], "Gitea rejected the operation",
            "the documented generic fallback, not a missing reason"
        );
        request.await.expect("request task");
    }

    #[tokio::test]
    async fn an_unreadable_success_body_remains_an_error() {
        // The counterpart, and why the previous test is not simply "never
        // fail": on a success the body was the point of the call, so silently
        // reporting an empty one would invent a result.
        let (base_url, request) = loopback_response(
            "200 OK",
            "application/json",
            b"",
            Some(MAX_OPERATION_RESPONSE_BYTES + 1),
        )
        .await;
        let operation =
            crate::catalog::exposed_operation("repository.get").expect("repository get operation");
        let arguments = Map::from_iter([
            ("owner".to_string(), json!("alice")),
            ("repo".to_string(), json!("demo")),
        ]);

        let Err(error) = client(&base_url)
            .execute_operation(operation, arguments)
            .await
        else {
            panic!("an unreadable success is an error")
        };
        assert_eq!(
            error.call_outcome(),
            crate::CallOutcome::Completed { status: 200 }
        );
        request.await.expect("request task");
    }

    #[tokio::test]
    async fn a_refusal_echoing_the_service_token_does_not_carry_it_back() {
        // The client sends the service token on every request, so any refusal
        // body is a place it could come back. Redacting only caller-supplied
        // argument values would miss it, and the normalized message is durable:
        // it becomes OperationResponse.data and reaches the model.
        const SERVICE_TOKEN: &str = "gta-not-a-real-token-8f2c1d";
        let (base_url, request) = loopback_response(
            "403 Forbidden",
            "application/json",
            br#"{"message":"token gta-not-a-real-token-8f2c1d lacks scope write:repository"}"#,
            None,
        )
        .await;
        let operation =
            crate::catalog::exposed_operation("repository.get").expect("repository get operation");
        let arguments = Map::from_iter([
            ("owner".to_string(), json!("alice")),
            ("repo".to_string(), json!("demo")),
        ]);

        let response = GiteaClient::new(&base_url, SERVICE_TOKEN, Duration::from_secs(1))
            .expect("client")
            .execute_operation(operation, arguments)
            .await
            .expect("a 403 is a structured refusal, not an error");
        assert_eq!(response.status, 403);
        let message = response.data["message"].as_str().expect("a reason");
        assert!(
            !message.contains(SERVICE_TOKEN),
            "the service token must not survive into the result: {message}"
        );
        assert!(
            message.contains("[REDACTED]") && message.contains("write:repository"),
            "the rest of the reason still reaches the caller: {message}"
        );
        request.await.expect("request task");
    }

    #[tokio::test]
    async fn a_request_that_went_out_unanswered_is_not_called_safe_to_retry() {
        // The dangerous direction. Gitea accepts the connection, reads the
        // DELETE, and never answers — which is what a mutation that ran behind
        // a dropped connection looks like from here. Classifying that as "not
        // sent" would tell a caller the repository is still there.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("listener address");
        let upstream = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request");
            read_request(&mut socket).await;
            // Hold the socket open past the client timeout without replying.
            tokio::time::sleep(Duration::from_secs(3)).await;
        });
        let operation = crate::catalog::exposed_operation("repository.delete")
            .expect("repository delete operation");
        let arguments = Map::from_iter([
            ("owner".to_string(), json!("alice")),
            ("repo".to_string(), json!("demo")),
        ]);

        let Err(error) = client(&format!("http://{address}"))
            .execute_operation(operation, arguments)
            .await
        else {
            panic!("an unanswered request cannot succeed")
        };
        assert_eq!(
            error.call_outcome(),
            crate::CallOutcome::Unknown,
            "the request reached Gitea, so the outcome is unknown rather than none: {error:?}"
        );
        upstream.abort();
    }

    #[tokio::test]
    async fn a_request_that_never_connected_is_safe_to_retry() {
        // The counterpart, and why the classification cannot just call every
        // transport failure unknown: nothing listens here, so nothing ran, and
        // a caller should be free to correct and reissue.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("listener address");
        drop(listener);
        let operation = crate::catalog::exposed_operation("repository.delete")
            .expect("repository delete operation");
        let arguments = Map::from_iter([
            ("owner".to_string(), json!("alice")),
            ("repo".to_string(), json!("demo")),
        ]);

        let Err(error) = client(&format!("http://{address}"))
            .execute_operation(operation, arguments)
            .await
        else {
            panic!("a refused connection cannot succeed")
        };
        assert_eq!(
            error.call_outcome(),
            crate::CallOutcome::NotSent,
            "a refused connection delivered nothing: {error:?}"
        );
    }

    #[test]
    fn only_a_failure_after_the_status_claims_the_call_completed() {
        // Guards the classification against the two ways it could quietly go
        // wrong: telling a caller nothing happened when it might have, and
        // claiming a settled outcome that was never observed.
        assert_eq!(
            ApiError::ResponseTooLarge.call_outcome(),
            crate::CallOutcome::Unknown,
            "a response fault without a status cannot claim the call is settled"
        );
        assert_eq!(
            ApiError::InvalidOperation.call_outcome(),
            crate::CallOutcome::NotSent
        );
        assert_eq!(
            ApiError::InvalidArguments("owner".to_string()).call_outcome(),
            crate::CallOutcome::NotSent
        );
        assert_eq!(
            ApiError::Upstream {
                status: 409,
                message: "name already taken".to_string(),
            }
            .call_outcome(),
            crate::CallOutcome::Completed { status: 409 },
            "a refusal is a settled outcome, not an unknown one"
        );
    }

    #[test]
    fn a_defect_in_this_repository_does_not_unsay_that_the_call_ran() {
        // `declared_response_headers` raises `InvalidOperation` when the
        // generated catalog declares a malformed header name, and it runs after
        // the status has arrived. The defect is ours, but the deletion it
        // interrupted still happened, so the outcome must not read "never sent".
        let error = ApiError::InvalidOperation.delivered_with(200);
        assert_eq!(
            error.call_outcome(),
            crate::CallOutcome::Completed { status: 200 },
            "how far the call got does not depend on what went wrong afterwards"
        );
        // The diagnosis is not traded away for the outcome.
        assert!(
            matches!(
                &error,
                ApiError::ResponseNotDelivered { source, .. }
                    if matches!(**source, ApiError::InvalidOperation)
            ),
            "the original error survives as the source: {error:?}"
        );
        assert!(error.to_string().contains("generated Gitea operation"));
    }

    #[test]
    fn a_status_already_recorded_is_not_overwritten() {
        // Two wrappings would bury the status the call actually answered with
        // behind whichever one ran last.
        let once = ApiError::ResponseTooLarge.delivered_with(201);
        let twice = once.delivered_with(500);
        assert_eq!(
            twice.call_outcome(),
            crate::CallOutcome::Completed { status: 201 }
        );
    }

    fn client(base_url: &str) -> GiteaClient {
        GiteaClient::new(base_url, "test-token", Duration::from_secs(1)).expect("client")
    }

    fn assert_service_token(request: &str) {
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: token test-token\r\n")
        );
    }

    async fn loopback_response(
        status: &'static str,
        content_type: &'static str,
        body: &'static [u8],
        declared_length: Option<usize>,
    ) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
        loopback_response_with_headers(status, content_type, body, declared_length, "").await
    }

    async fn loopback_response_with_headers(
        status: &'static str,
        content_type: &'static str,
        body: &'static [u8],
        declared_length: Option<usize>,
        extra_headers: &'static str,
    ) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("listener address");
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request");
            let request = read_request(&mut socket).await;
            let length = declared_length.unwrap_or(body.len());
            let headers = format!(
                "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\n{extra_headers}content-length: {length}\r\nconnection: close\r\n\r\n"
            );
            socket
                .write_all(headers.as_bytes())
                .await
                .expect("response headers");
            socket.write_all(body).await.expect("response body");
            request
        });
        (format!("http://{address}"), task)
    }

    async fn loopback_response_with_raw_header(
        status: &'static str,
        body: &'static [u8],
        raw_header: &'static [u8],
    ) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("listener address");
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request");
            let request = read_request(&mut socket).await;
            let mut response =
                format!("HTTP/1.1 {status}\r\ncontent-type: application/json\r\n").into_bytes();
            response.extend_from_slice(raw_header);
            response.extend_from_slice(
                format!(
                    "content-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            );
            response.extend_from_slice(body);
            socket.write_all(&response).await.expect("raw response");
            request
        });
        (format!("http://{address}"), task)
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let header_end = loop {
            if let Some(offset) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break offset + 4;
            }
            let mut chunk = [0_u8; 4096];
            let length = socket.read(&mut chunk).await.expect("request headers");
            assert_ne!(length, 0, "connection closed before request headers");
            request.extend_from_slice(&chunk[..length]);
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.split_once(':').and_then(|(name, value)| {
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("content length"))
                })
            })
            .unwrap_or(0);
        while request.len() < header_end + content_length {
            let mut chunk = [0_u8; 4096];
            let length = socket.read(&mut chunk).await.expect("request body");
            assert_ne!(length, 0, "connection closed before request body");
            request.extend_from_slice(&chunk[..length]);
        }
        request
    }
}
