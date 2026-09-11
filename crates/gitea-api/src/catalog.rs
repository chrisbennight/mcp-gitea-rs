use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::{Map, Value};

const CATALOG_JSON: &str = include_str!("../../../generated/gitea-1.26.4-operations.json");

#[derive(Debug, Deserialize)]
pub struct OperationCatalog {
    pub schema_version: u32,
    pub gitea_version: String,
    pub source_sha256: String,
    pub operation_count: usize,
    pub exposed_operation_count: usize,
    pub operations: Vec<OperationSpec>,
    pub compatibility_ledger: Vec<CompatibilityEntry>,
}

#[derive(Debug, Deserialize)]
pub struct CompatibilityEntry {
    pub operation_id: String,
    pub tool_name: String,
    pub state: String,
    pub replacement: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct OperationSpec {
    pub tool_name: String,
    pub operation_id: String,
    pub tag: String,
    pub tags: Vec<String>,
    pub summary: String,
    pub agent_guidance: Option<String>,
    pub method: String,
    pub path: String,
    pub consumes: Vec<String>,
    pub produces: Vec<String>,
    pub parameters: Vec<ParameterSpec>,
    pub response_headers: Vec<String>,
    pub input_schema: Map<String, Value>,
    pub risk: OperationRisk,
    pub administrative: bool,
    pub open_world: OpenWorld,
    pub secret_result: bool,
    pub auth_lane: AuthLane,
    pub exposed: bool,
    /// The specification's own deprecation flag. Deprecated operations stay
    /// dispatchable but are not published as tools or served by discovery.
    pub deprecated: Deprecation,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Deprecation(bool);

impl Deprecation {
    #[must_use]
    pub const fn is_deprecated(self) -> bool {
        self.0
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct OpenWorld(bool);

impl OpenWorld {
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        self.0
    }
}

#[derive(Debug, Deserialize)]
pub struct ParameterSpec {
    pub name: String,
    pub location: ParameterLocation,
    pub required: bool,
    #[serde(rename = "type")]
    pub value_type: Option<String>,
    pub format: Option<String>,
    pub collection_format: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationRisk {
    Read,
    Mutation,
    Destructive,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ParameterLocation {
    Path,
    Query,
    Header,
    Body,
    FormData,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthLane {
    ServicePat,
    Basic,
}

#[must_use]
/// Return the checked-in Gitea operation catalog.
///
/// # Panics
///
/// Panics when the generated catalog is not valid JSON matching the catalog
/// model. Generator checks and catalog tests prevent an invalid artifact from
/// reaching a release build.
pub fn operation_catalog() -> &'static OperationCatalog {
    static CATALOG: OnceLock<OperationCatalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(CATALOG_JSON)
            .expect("checked-in generated Gitea operation catalog must be valid")
    })
}

#[must_use]
pub fn exposed_operation(tool_name: &str) -> Option<&'static OperationSpec> {
    operation_catalog()
        .operations
        .iter()
        .find(|operation| operation.exposed && operation.tool_name == tool_name)
}

#[must_use]
pub fn exposed_operation_by_id(operation_id: &str) -> Option<&'static OperationSpec> {
    operation_catalog()
        .operations
        .iter()
        .find(|operation| operation.exposed && operation.operation_id == operation_id)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn catalog_is_complete_and_names_are_unique() {
        let catalog = operation_catalog();
        assert_eq!(catalog.schema_version, 2);
        assert_eq!(catalog.gitea_version, "1.26.4");
        assert_eq!(catalog.operation_count, 471);
        assert_eq!(catalog.operations.len(), catalog.operation_count);
        assert_eq!(
            catalog
                .operations
                .iter()
                .filter(|operation| operation.exposed)
                .count(),
            catalog.exposed_operation_count
        );
        let names: HashSet<_> = catalog
            .operations
            .iter()
            .map(|operation| &operation.tool_name)
            .collect();
        assert_eq!(names.len(), catalog.operation_count);
    }

    #[test]
    fn basic_auth_operations_are_explicitly_covered() {
        let catalog = operation_catalog();
        let covered: HashSet<_> = catalog
            .compatibility_ledger
            .iter()
            .filter(|entry| entry.state == "covered_basic_auth" && entry.replacement.is_some())
            .map(|entry| entry.operation_id.as_str())
            .collect();
        assert_eq!(
            covered,
            HashSet::from(["userCreateToken", "userDeleteAccessToken", "userGetTokens"])
        );
    }

    #[test]
    fn stable_operation_id_lookup_is_independent_of_curated_tool_names() {
        let operation = exposed_operation_by_id("repoCreateKey").expect("operation");
        assert_eq!(operation.tool_name, "repository.create_deploy_key");
    }
}
