#!/usr/bin/env python3
"""Generate the deterministic Gitea operation catalog from pinned Swagger 2."""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import re
import sys
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "openapi" / "gitea-1.26.4.json"
OUTPUT = ROOT / "generated" / "gitea-1.26.4-operations.json"
HTTP_METHODS = ("get", "post", "put", "patch", "delete")
PREFIX_BY_TAG = {
    "admin": ("admin_",),
    "issue": ("issue_",),
    "notification": ("notify_", "notification_"),
    "organization": ("org_", "organization_"),
    "repository": ("repo_", "repository_"),
    "user": ("user_",),
}
CURATED_TOOL_NAMES = {
    "adminCreateRepo": "admin.create_user_repository",
    "createCurrentUserRepo": "repository.create_for_current_user",
    "createOrgRepo": "organization.create_repository",
    "deleteAdminRunner": "admin.delete_actions_runner",
    "deleteOrgRunner": "organization.delete_actions_runner",
    "deleteRepoRunner": "repository.delete_actions_runner",
    "deleteUserRunner": "user.delete_actions_runner",
    "getAdminRunner": "admin.get_actions_runner",
    "getAdminRunners": "admin.list_actions_runners",
    "getOrgRunner": "organization.get_actions_runner",
    "getOrgRunners": "organization.list_actions_runners",
    "getRepoRunner": "repository.get_actions_runner",
    "getRepoRunners": "repository.list_actions_runners",
    "getUserRunner": "user.get_actions_runner",
    "getUserRunners": "user.list_actions_runners",
    "notifyReadList": "notification.mark_threads",
    "notifyReadRepoList": "notification.mark_repository_threads",
    "notifyReadThread": "notification.mark_thread",
    "repoCreateKey": "repository.create_deploy_key",
    "repoDeleteKey": "repository.delete_deploy_key",
    "repoGetKey": "repository.get_deploy_key",
    "repoListKeys": "repository.list_deploy_keys",
    "repoUpdatePullRequest": "repository.update_pull_request_branch",
    "updateAdminRunner": "admin.update_actions_runner",
    "updateOrgRunner": "organization.update_actions_runner",
    "updateRepoRunner": "repository.update_actions_runner",
    "updateUserRunner": "user.update_actions_runner",
}
#: Summaries for operations whose upstream text does not distinguish them from
#: another exposed operation. Gitea writes one summary per handler without
#: regard to scope, so "Get a hook" arrives four times — instance, organization,
#: repository, and user — and an agent choosing from summaries alone has no
#: basis to prefer one. Each entry names whatever actually differs: the scope
#: that owns the resource, the deprecated path, or the transport form.
#:
#: Only ambiguous operations appear here. A short summary that already
#: identifies its operation, such as "Delete a repository", is left alone;
#: brevity is not the defect.
CURATED_SUMMARIES = {
    # Webhooks: identical text across four owning scopes.
    "adminCreateHook": "Create an instance-wide system webhook",
    "adminDeleteHook": "Delete an instance-wide system webhook",
    "adminEditHook": "Update an instance-wide system webhook",
    "adminGetHook": "Get an instance-wide system webhook",
    "orgCreateHook": "Create an organization webhook",
    "orgDeleteHook": "Delete an organization webhook",
    "orgEditHook": "Update an organization webhook",
    "orgGetHook": "Get an organization webhook",
    "repoCreateHook": "Create a repository webhook",
    "repoGetHook": "Get a repository webhook",
    "userCreateHook": "Create a webhook owned by the signed-in user",
    "userDeleteHook": "Delete a webhook owned by the signed-in user",
    "userEditHook": "Update a webhook owned by the signed-in user",
    "userGetHook": "Get a webhook owned by the signed-in user",
    # Avatars: identical text across three owning scopes.
    "orgDeleteAvatar": "Delete an organization's avatar",
    "orgUpdateAvatar": "Update an organization's avatar",
    "repoDeleteAvatar": "Delete a repository's avatar",
    "repoUpdateAvatar": "Update a repository's avatar",
    "userDeleteAvatar": "Delete the signed-in user's avatar",
    "userUpdateAvatar": "Update the signed-in user's avatar",
    # Labels: repository-scoped and organization-scoped sets share text.
    "issueDeleteLabel": "Delete a repository label",
    "issueEditLabel": "Update a repository label",
    "issueGetLabel": "Get a repository label",
    "orgDeleteLabel": "Delete an organization label",
    "orgEditLabel": "Update an organization label",
    "orgGetLabel": "Get an organization label",
    # Blocking: organization-scoped and account-scoped block lists.
    "organizationBlockUser": "Block a user from an organization",
    "organizationUnblockUser": "Unblock a user from an organization",
    "userBlockUser": "Block a user for the signed-in account",
    "userUnblockUser": "Unblock a user for the signed-in account",
    # Organization creation: administrative on behalf of a user, or ordinary.
    "adminCreateOrg": "Create an organization owned by a given user",
    "orgCreate": "Create an organization owned by the signed-in user",
    # Deprecated variants that still carry their replacement's summary.
    "createOrgRepoDeprecated": "Create a repository in an organization (deprecated path)",
    "issueDeleteComment": "Delete an issue or pull request comment",
    "issueDeleteCommentDeprecated": "Delete an issue comment (deprecated path)",
    "issueEditComment": "Update an issue or pull request comment",
    "issueEditCommentDeprecated": "Update an issue comment (deprecated path)",
    # Distinct resources that upstream described with one sentence. Only the
    # comments reader is curated: upstream's wording is already accurate for
    # the review itself, and restating it would add nothing.
    "repoGetPullReviewComments": "List the comments on one pull request review",
    "repoListAllGitRefs": "List every Git ref in a repository",
    "repoListGitRefs": "Get one Git ref, or the refs under a prefix",
    # Same resource, different request form.
    "repoGetFileContents": "Get metadata and contents of several files, listed in the query",
    "repoGetFileContentsPost": "Get metadata and contents of several files, listed in the body",
}
CURATED_AGENT_GUIDANCE = {
    "ActionsDispatchWorkflow": (
        'Use to run a Gitea Actions workflow by file name or workflow ID on a ref. '
        'Example input: {"owner":"acme","repo":"widget","workflow_id":"deploy.yml",'
        '"body":{"ref":"refs/heads/main","inputs":{"environment":"staging"}}}.'
    ),
    "adminCreateOrg": (
        "Use when an instance administrator must create an organization on behalf "
        "of a user; ordinary organization creation uses organization.create."
    ),
    "adminCreateRepo": (
        'Use when an instance administrator must create a repository for another '
        'user. Example input: {"username":"alice","repository":{"name":"widget",'
        '"private":true,"auto_init":true}}.'
    ),
    "adminCreateUser": (
        "Use for instance-level user provisioning. This is distinct from "
        "organization membership and repository collaboration."
    ),
    "createCurrentUserRepo": (
        'Use to create a repository owned by the configured service identity. '
        'Example input: {"body":{"name":"widget","private":true,"auto_init":true}}.'
    ),
    "createOrgRepo": (
        'Use to create a repository in an existing organization. Example input: '
        '{"org":"acme","body":{"name":"widget","private":true,"auto_init":true}}.'
    ),
    "getAdminRunners": (
        "Use to inventory instance-wide Gitea Actions runners, including online, "
        "busy, disabled, and label state."
    ),
    "getOrgRunners": (
        "Use to inventory Gitea Actions runners registered to one organization."
    ),
    "getRepoRunners": (
        "Use to inventory Gitea Actions runners registered to one repository."
    ),
    "getWorkflowRuns": (
        "Use to list Gitea Actions workflow runs for a repository; filter by "
        "branch, event, status, actor, or commit when needed."
    ),
    "getUserRunners": (
        "Use to inventory Gitea Actions runners available to the configured "
        "user, including online, busy, disabled, and label state."
    ),
    "linkPackage": (
        'Use to associate an existing owner package with a repository. Example '
        'input: {"owner":"acme","type":"container","name":"widget",'
        '"repo_name":"widget"}.'
    ),
    "listPackages": (
        "Use to discover container, Maven, npm, PyPI, Cargo, and other packages "
        "owned by a user or organization."
    ),
    "notifyGetList": (
        "Use to list the configured identity's notification inbox; filters include "
        "read state, subject type, and time bounds."
    ),
    "notifyReadList": (
        "Use to mark a filtered set of notification threads read, unread, or "
        "pinned. Omitted filters can affect the whole inbox."
    ),
    "notifyReadRepoList": (
        "Use to mark notification threads for one repository read, unread, or "
        "pinned."
    ),
    "notifyReadThread": (
        "Use to change read or pinned state for one notification thread by ID."
    ),
    "orgAddTeamMember": (
        'Use to add a user to an existing organization team. Example input: '
        '{"id":42,"username":"alice"}.'
    ),
    "orgAddTeamRepository": (
        'Use to grant an existing team access to an organization repository. '
        'Example input: {"id":42,"org":"acme","repo":"widget"}.'
    ),
    "orgCreate": (
        'Use to create an organization as the configured identity. Example input: '
        '{"organization":{"username":"acme","visibility":"private"}}.'
    ),
    "orgCreateTeam": (
        'Use to create a permission-bearing organization team. Example input: '
        '{"org":"acme","body":{"name":"developers","permission":"write",'
        '"units":["repo.code","repo.pulls","repo.actions"]}}.'
    ),
    "repoAddCollaborator": (
        'Use to add or change a repository collaborator permission. Example input: '
        '{"owner":"acme","repo":"widget","collaborator":"alice",'
        '"body":{"permission":"write"}}.'
    ),
    "repoAddTeam": (
        'Use to grant an organization team access to a repository when the team '
        'name is known. Example input: {"owner":"acme","repo":"widget",'
        '"team":"developers"}.'
    ),
    "repoCreateBranchProtection": (
        'Use to create merge guards, required status checks, approval rules, push '
        'restrictions, or signed-commit requirements. Example input: '
        '{"owner":"acme","repo":"widget","body":{"rule_name":"main",'
        '"enable_status_check":true,"status_check_contexts":["test / test"],'
        '"required_approvals":1}}.'
    ),
    "repoCreateHook": (
        'Use to install a repository webhook. Example input: {"owner":"acme",'
        '"repo":"widget","body":{"type":"gitea","active":true,"events":["push"],'
        '"config":{"url":"https://hooks.example.test","content_type":"json"}}}.'
    ),
    "repoCreateKey": (
        "Use to add an SSH deploy key to a repository. The body requires title and "
        "public key and can select read-only or read/write access."
    ),
    "repoCreatePullRequest": (
        'Use to open a pull request from a head branch into a base branch. Example '
        'input: {"owner":"acme","repo":"widget","body":{"title":"Ship widget",'
        '"head":"feature/widget","base":"main"}}.'
    ),
    "repoCreateRelease": (
        'Use to create a release and optionally its Git tag. Example input: '
        '{"owner":"acme","repo":"widget","body":{"tag_name":"v1.2.3",'
        '"name":"v1.2.3","target_commitish":"main"}}.'
    ),
    "repoEdit": (
        'Use to change repository settings such as default branch, visibility, '
        'merge methods, Actions, issues, packages, and branch deletion. Example '
        'input: {"owner":"acme","repo":"widget","body":{"default_branch":"main",'
        '"has_actions":true,"default_delete_branch_after_merge":true}}.'
    ),
    "repoEditBranchProtection": (
        "Use to replace selected settings on an existing branch-protection rule "
        "identified by its rule name."
    ),
    "repoGet": (
        "Use for repository details and settings: visibility, default branch, "
        "enabled features, merge policy, permissions, and clone URLs."
    ),
    "repoMergePullRequest": (
        "Use to merge a pull request after independently verifying review and "
        "status gates. The body selects merge, rebase, squash, or fast-forward."
    ),
    "repoUpdatePullRequest": (
        "Use to bring a pull request head branch up to date with its base branch. "
        "This does not edit PR title/body and does not merge the PR."
    ),
    "repoUpdateTopics": (
        'Use to replace the complete repository topic set. Example input: '
        '{"owner":"acme","repo":"widget","body":{"topics":["rust","mcp"]}}.'
    ),
}
BASIC_AUTH_OPERATIONS = {
    "userCreateToken",
    "userDeleteAccessToken",
    "userGetTokens",
}
BASIC_AUTH_REPLACEMENTS = {
    "userCreateToken": "access_token.create",
    "userDeleteAccessToken": "access_token.revoke",
    "userGetTokens": "access_token.list",
}
SECRET_RESULT_OPERATIONS = {
    "adminCreateRunnerRegistrationToken",
    "getVerificationToken",
    "orgCreateRunnerRegistrationToken",
    "repoCreateRunnerRegistrationToken",
    "userCreateRunnerRegistrationToken",
    "userCreateToken",
    "userCreateOAuth2Application",
    "userGetOAuth2Application",
    "userGetOauth2Application",
    "userUpdateOAuth2Application",
}
NON_DELETE_DESTRUCTIVE_OPERATIONS = {
    "ActionsDispatchWorkflow",
    "acceptRepoTransfer",
    "adminAdoptRepository",
    "adminCronRun",
    "adminRenameUser",
    "issueEditIssueDeadline",
    "issueStartStopWatch",
    "issueStopStopWatch",
    "linkPackage",
    "orgUpdateAvatar",
    "pinIssue",
    "rejectRepoTransfer",
    "renameOrg",
    "repoApplyDiffPatch",
    "repoChangeFiles",
    "repoDismissPullReview",
    "repoMergePullRequest",
    "repoMergeUpstream",
    "repoMirrorSync",
    "repoPushMirrorSync",
    "repoResolvePullReviewComment",
    "repoSubmitPullReview",
    "repoTestHook",
    "repoTransfer",
    "repoUnDismissPullReview",
    "repoUnresolvePullReviewComment",
    "repoUpdateAvatar",
    "repoUpdateBranchProtectionPriories",
    "repoUpdatePullRequest",
    "rerunFailedWorkflowRun",
    "rerunWorkflowJob",
    "rerunWorkflowRun",
    "unlinkPackage",
    "userUpdateAvatar",
    "userVerifyGPGKey",
}
ADDITIVE_PUT_OPERATIONS = {
    "issueAddSubscription",
    "orgAddTeamMember",
    "orgAddTeamRepository",
    "repoAddTeam",
    "repoAddTopic",
    "userCurrentPutFollow",
    "userCurrentPutStar",
    "userCurrentPutSubscription",
}
OPEN_WORLD_OPERATIONS = {
    "ActionsDispatchWorkflow",
    "repoAddPushMirror",
    "repoMigrate",
    "repoMirrorSync",
    "repoPushMirrorSync",
    "repoTestHook",
    "rerunFailedWorkflowRun",
    "rerunWorkflowJob",
    "rerunWorkflowRun",
}
READ_ONLY_POST_OPERATIONS = {
    "renderMarkdown",
    "renderMarkdownRaw",
    "renderMarkup",
    "repoGetFileContentsPost",
}
SENSITIVE_RESPONSE_FIELDS = {
    "auth_password",
    "auth_token",
    "authorization_header",
    "aws_secret_access_key",
    "client_secret",
    "password",
    "remote_password",
    "secret",
    "token",
}
SCHEMA_KEYS = {
    "default",
    "enum",
    "exclusiveMaximum",
    "exclusiveMinimum",
    "format",
    "items",
    "maxItems",
    "maxLength",
    "maximum",
    "minItems",
    "minLength",
    "minimum",
    "multipleOf",
    "pattern",
    "type",
    "uniqueItems",
}


def snake_case(value: str) -> str:
    first = re.sub(r"(.)([A-Z][a-z]+)", r"\1_\2", value)
    return re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", first).replace("-", "_").lower()


def generated_tool_name(operation_id: str, tag: str) -> str:
    operation = snake_case(operation_id)
    for prefix in PREFIX_BY_TAG.get(tag, ()):
        if operation.startswith(prefix):
            operation = operation[len(prefix) :]
            break
    return f"{snake_case(tag)}.{operation}"


def tool_name(operation_id: str, tag: str) -> str:
    return CURATED_TOOL_NAMES.get(
        operation_id, generated_tool_name(operation_id, tag)
    )


def resolve_parameter(specification: dict[str, Any], parameter: dict[str, Any]) -> dict[str, Any]:
    reference = parameter.get("$ref")
    if reference is None:
        return copy.deepcopy(parameter)
    prefix = "#/parameters/"
    if not reference.startswith(prefix):
        raise ValueError(f"unsupported parameter reference: {reference}")
    name = reference.removeprefix(prefix)
    return copy.deepcopy(specification["parameters"][name])


def parameter_schema(parameter: dict[str, Any]) -> dict[str, Any]:
    location = parameter["in"]
    if location == "body":
        schema = copy.deepcopy(parameter.get("schema", {}))
    elif location == "formData" and parameter.get("type") == "file":
        schema = {
            "type": "object",
            "description": (
                "Binary upload encoded for MCP transport. content_base64 must contain "
                "unpadded or padded standard Base64."
            ),
            "properties": {
                "filename": {"type": "string", "minLength": 1, "maxLength": 255},
                "media_type": {"type": "string", "minLength": 1, "maxLength": 255},
                "content_base64": {"type": "string", "minLength": 1},
            },
            "required": ["filename", "content_base64"],
            "additionalProperties": False,
        }
    else:
        schema = {
            key: copy.deepcopy(value)
            for key, value in parameter.items()
            if key in SCHEMA_KEYS
        }
    description = parameter.get("description")
    if description and "description" not in schema:
        schema["description"] = description
    return close_object_schemas(schema)


def referenced_definitions(value: Any) -> set[str]:
    found: set[str] = set()
    if isinstance(value, dict):
        reference = value.get("$ref")
        if isinstance(reference, str) and reference.startswith("#/definitions/"):
            found.add(reference.removeprefix("#/definitions/"))
        for nested in value.values():
            found.update(referenced_definitions(nested))
    elif isinstance(value, list):
        for nested in value:
            found.update(referenced_definitions(nested))
    return found


def attach_definitions(
    specification: dict[str, Any], input_schema: dict[str, Any]
) -> dict[str, Any]:
    definitions = specification.get("definitions", {})
    pending = list(referenced_definitions(input_schema))
    selected: dict[str, Any] = {}
    while pending:
        name = pending.pop()
        if name in selected:
            continue
        if name not in definitions:
            raise ValueError(f"missing referenced definition: {name}")
        definition = close_object_schemas(copy.deepcopy(definitions[name]))
        selected[name] = definition
        pending.extend(referenced_definitions(definition) - selected.keys())
    if selected:
        input_schema["definitions"] = {
            name: selected[name] for name in sorted(selected)
        }
    return input_schema


#: Swagger vendor extensions that name Go types rather than describe the API.
#: They survive into every generated schema, and the schemas are what an agent
#: reads when it decides which tool to call and how to fill its arguments, so
#: they are paid for on every request that carries the tool list.
#:
#: ``x-go-name`` is the Go struct field behind a property whose JSON name the
#: schema already states, and ``x-go-package`` is the Go import path of the type
#: behind a definition. Neither says anything about the request contract, and a
#: property called ``badge_slugs`` also announcing itself as ``BadgeSlugs`` can
#: only mislead argument naming.
#:
#: ``x-go-enum-desc`` is deliberately excluded: it explains what each enum value
#: means, which is the one thing in this family a caller actually needs.
GO_CODEGEN_EXTENSIONS = frozenset({"x-go-name", "x-go-package"})


def strip_go_codegen_extensions(value: Any) -> Any:
    """Remove Go codegen vendor extensions from a schema or parameter tree."""
    if isinstance(value, dict):
        return {
            key: strip_go_codegen_extensions(nested)
            for key, nested in value.items()
            if key not in GO_CODEGEN_EXTENSIONS
        }
    if isinstance(value, list):
        return [strip_go_codegen_extensions(nested) for nested in value]
    return value


def close_object_schemas(value: Any) -> Any:
    if isinstance(value, dict):
        for key, nested in list(value.items()):
            value[key] = close_object_schemas(nested)
        if (
            value.get("type") == "object" or "properties" in value
        ) and "additionalProperties" not in value:
            value["additionalProperties"] = False
    elif isinstance(value, list):
        for index, nested in enumerate(value):
            value[index] = close_object_schemas(nested)
    return value


def operation_parameters(
    specification: dict[str, Any],
    path_item: dict[str, Any],
    operation: dict[str, Any],
) -> list[dict[str, Any]]:
    combined = [
        resolve_parameter(specification, parameter)
        for parameter in (
            path_item.get("parameters", []) + operation.get("parameters", [])
        )
    ]
    names = [parameter["name"] for parameter in combined]
    if len(names) != len(set(names)):
        raise ValueError(f"duplicate operation parameter names: {names}")
    return combined


def build_input_schema(
    specification: dict[str, Any], parameters: list[dict[str, Any]]
) -> dict[str, Any]:
    properties = {
        parameter["name"]: parameter_schema(parameter) for parameter in parameters
    }
    required = [
        parameter["name"] for parameter in parameters if parameter.get("required")
    ]
    schema: dict[str, Any] = {
        "type": "object",
        "properties": properties,
        "additionalProperties": False,
    }
    if required:
        schema["required"] = required
    return attach_definitions(specification, schema)


def compatibility(operation_id: str) -> tuple[bool, str, str | None]:
    if operation_id == "getVersion":
        return False, "covered", "server.version"
    if operation_id in BASIC_AUTH_OPERATIONS:
        return False, "covered_basic_auth", BASIC_AUTH_REPLACEMENTS[operation_id]
    return True, "supported", None


def resolve_response(
    specification: dict[str, Any], response: dict[str, Any]
) -> dict[str, Any]:
    reference = response.get("$ref")
    if reference is None:
        return response
    prefix = "#/responses/"
    if not reference.startswith(prefix):
        raise ValueError(f"unsupported response reference: {reference}")
    return specification["responses"][reference.removeprefix(prefix)]


def successful_responses(
    specification: dict[str, Any], operation: dict[str, Any]
) -> list[dict[str, Any]]:
    return [
        resolve_response(specification, response)
        for status, response in operation.get("responses", {}).items()
        if status.isdigit() and 200 <= int(status) < 300
    ]


def response_headers(
    specification: dict[str, Any], operation: dict[str, Any]
) -> list[str]:
    return sorted(
        {
            name
            for response in successful_responses(specification, operation)
            for name in response.get("headers", {})
        },
        key=str.lower,
    )


def sensitive_field_name(name: str) -> bool:
    normalized = snake_case(name)
    return (
        normalized in SENSITIVE_RESPONSE_FIELDS
        or normalized.endswith("_password")
        or normalized.endswith("_secret")
    )


def schema_contains_sensitive_field(
    specification: dict[str, Any],
    schema: Any,
    visited_definitions: set[str] | None = None,
) -> bool:
    if not isinstance(schema, dict):
        return False
    visited = visited_definitions or set()
    reference = schema.get("$ref")
    if isinstance(reference, str) and reference.startswith("#/definitions/"):
        name = reference.removeprefix("#/definitions/")
        if name in visited:
            return False
        return schema_contains_sensitive_field(
            specification,
            specification["definitions"][name],
            visited | {name},
        )
    properties = schema.get("properties", {})
    if any(sensitive_field_name(name) for name in properties):
        return True
    return any(
        schema_contains_sensitive_field(specification, nested, visited)
        for nested in (
            list(properties.values())
            + ([schema["items"]] if "items" in schema else [])
            + (
                [schema["additionalProperties"]]
                if isinstance(schema.get("additionalProperties"), dict)
                else []
            )
            + list(schema.get("allOf", []))
        )
    )


def operation_has_sensitive_result(
    specification: dict[str, Any],
    operation: dict[str, Any],
    operation_id: str,
) -> bool:
    if operation_id in SECRET_RESULT_OPERATIONS:
        return True
    for response in successful_responses(specification, operation):
        if any(sensitive_field_name(name) for name in response.get("headers", {})):
            return True
        if schema_contains_sensitive_field(
            specification, response.get("schema", {})
        ):
            return True
    return False


def parameter_metadata(parameter: dict[str, Any]) -> dict[str, Any]:
    metadata = {
        "name": parameter["name"],
        "location": parameter["in"],
        "required": bool(parameter.get("required")),
    }
    if "type" in parameter:
        metadata["type"] = parameter["type"]
    if "format" in parameter:
        metadata["format"] = parameter["format"]
    if "collectionFormat" in parameter:
        metadata["collection_format"] = parameter["collectionFormat"]
    return metadata


def generate(source_bytes: bytes) -> dict[str, Any]:
    specification = json.loads(source_bytes)
    if specification.get("swagger") != "2.0":
        raise ValueError("only Swagger 2.0 is supported")
    version = specification.get("info", {}).get("version")
    if version != "1.26.4":
        raise ValueError(f"unexpected Gitea specification version: {version}")

    operations: list[dict[str, Any]] = []
    ledger: list[dict[str, Any]] = []
    names: set[str] = set()
    for path in sorted(specification["paths"]):
        path_item = specification["paths"][path]
        for method in HTTP_METHODS:
            if method not in path_item:
                continue
            operation = path_item[method]
            operation_id = operation.get("operationId")
            if not operation_id:
                raise ValueError(f"{method.upper()} {path} has no operationId")
            tags = operation.get("tags") or ["miscellaneous"]
            tag = tags[0]
            name = tool_name(operation_id, tag)
            if name in names:
                raise ValueError(f"duplicate generated tool name: {name}")
            names.add(name)
            parameters = operation_parameters(specification, path_item, operation)
            exposed, state, replacement = compatibility(operation_id)
            risk = (
                "read"
                if method == "get" or operation_id in READ_ONLY_POST_OPERATIONS
                else "destructive"
                if method in {"delete", "patch"}
                or (method == "put" and operation_id not in ADDITIVE_PUT_OPERATIONS)
                or operation_id in NON_DELETE_DESTRUCTIVE_OPERATIONS
                else "mutation"
            )
            record = {
                "tool_name": name,
                "operation_id": operation_id,
                "tag": tag,
                "tags": tags,
                "summary": (
                    CURATED_SUMMARIES.get(operation_id)
                    or operation.get("summary")
                    or operation_id
                ),
                "method": method.upper(),
                "path": path,
                "consumes": operation.get(
                    "consumes", specification.get("consumes", ["application/json"])
                ),
                "produces": operation.get(
                    "produces", specification.get("produces", ["application/json"])
                ),
                "parameters": [
                    parameter_metadata(parameter) for parameter in parameters
                ],
                "response_headers": response_headers(specification, operation),
                "input_schema": strip_go_codegen_extensions(
                    build_input_schema(specification, parameters)
                ),
                "risk": risk,
                "administrative": path.startswith("/admin/") or tag == "admin",
                "open_world": operation_id in OPEN_WORLD_OPERATIONS,
                "secret_result": operation_has_sensitive_result(
                    specification, operation, operation_id
                ),
                "auth_lane": (
                    "basic" if operation_id in BASIC_AUTH_OPERATIONS else "service_pat"
                ),
                "exposed": exposed,
                # The specification's own deprecation flag. A deprecated
                # operation stays dispatchable for compatibility but is not
                # published or discovered, so it cannot compete with its
                # canonical sibling for selection.
                "deprecated": bool(operation.get("deprecated", False)),
            }
            if guidance := CURATED_AGENT_GUIDANCE.get(operation_id):
                record["agent_guidance"] = guidance
            operations.append(record)
            if not exposed:
                entry = {
                    "operation_id": operation_id,
                    "tool_name": name,
                    "state": state,
                }
                if replacement:
                    entry["replacement"] = replacement
                ledger.append(entry)

    operations.sort(
        key=lambda operation: generated_tool_name(
            operation["operation_id"], operation["tag"]
        )
    )
    ledger.sort(key=lambda entry: entry["operation_id"])
    if len(operations) != 471:
        raise ValueError(f"expected 471 operations, generated {len(operations)}")
    operation_ids = {operation["operation_id"] for operation in operations}
    unknown_curated = (
        CURATED_TOOL_NAMES.keys() | CURATED_AGENT_GUIDANCE.keys()
    ) - operation_ids
    if unknown_curated:
        raise ValueError(
            f"curated metadata references unknown operations: {sorted(unknown_curated)}"
        )
    classified_ids = (
        BASIC_AUTH_OPERATIONS
        | BASIC_AUTH_REPLACEMENTS.keys()
        | SECRET_RESULT_OPERATIONS
        | NON_DELETE_DESTRUCTIVE_OPERATIONS
        | ADDITIVE_PUT_OPERATIONS
        | OPEN_WORLD_OPERATIONS
        | READ_ONLY_POST_OPERATIONS
    )
    if missing := classified_ids - operation_ids:
        raise ValueError(f"classified operations are absent from the spec: {missing}")
    return {
        "schema_version": 2,
        "gitea_version": version,
        "source_sha256": hashlib.sha256(source_bytes).hexdigest(),
        "operation_count": len(operations),
        "exposed_operation_count": sum(
            1 for operation in operations if operation["exposed"]
        ),
        "operations": operations,
        "compatibility_ledger": ledger,
    }


def encoded_catalog(catalog: dict[str, Any]) -> bytes:
    return (
        json.dumps(catalog, indent=2, sort_keys=True, ensure_ascii=False) + "\n"
    ).encode()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--check",
        action="store_true",
        help="fail unless the checked-in catalog exactly matches regeneration",
    )
    arguments = parser.parse_args()
    generated = encoded_catalog(generate(SOURCE.read_bytes()))
    if arguments.check:
        if not OUTPUT.exists() or OUTPUT.read_bytes() != generated:
            print(f"{OUTPUT.relative_to(ROOT)} is stale", file=sys.stderr)
            return 1
        return 0
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    OUTPUT.write_bytes(generated)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
