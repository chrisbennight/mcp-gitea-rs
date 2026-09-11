import importlib.util
import json
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "generate_api", ROOT / "scripts" / "generate_api.py"
)
generate_api = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(generate_api)


class GenerateApiTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.catalog = generate_api.generate(generate_api.SOURCE.read_bytes())

    def operation(self, operation_id):
        return next(
            operation
            for operation in self.catalog["operations"]
            if operation["operation_id"] == operation_id
        )

    def assert_object_schemas_closed(self, value, path="$"):
        if isinstance(value, dict):
            if value.get("type") == "object" or "properties" in value:
                self.assertIn(
                    "additionalProperties",
                    value,
                    f"open object schema at {path}",
                )
            for name, nested in value.items():
                self.assert_object_schemas_closed(nested, f"{path}.{name}")
        elif isinstance(value, list):
            for index, nested in enumerate(value):
                self.assert_object_schemas_closed(nested, f"{path}[{index}]")

    def test_no_two_exposed_tools_share_a_summary(self):
        # The summary is what an agent reads when choosing between tools, so a
        # summary shared by two tools cannot inform that choice. Upstream ships
        # "Get a hook" for the instance, organization, repository, and user
        # webhook readers alike.
        seen = {}
        collisions = {}
        for operation in self.catalog["operations"]:
            if not operation["exposed"]:
                continue
            key = operation["summary"].strip().lower()
            if key in seen:
                collisions.setdefault(key, [seen[key]]).append(operation["tool_name"])
            else:
                seen[key] = operation["tool_name"]
        self.assertEqual(collisions, {}, f"summaries shared by several tools: {collisions}")

    def test_every_curated_summary_targets_a_real_operation(self):
        # A curated entry whose operation_id disappears upstream would silently
        # stop applying, and the collision it was written to resolve would
        # return with nothing here failing.
        known = {operation["operation_id"] for operation in self.catalog["operations"]}
        unknown = sorted(set(generate_api.CURATED_SUMMARIES) - known)
        self.assertEqual(unknown, [], f"curated summaries for unknown operations: {unknown}")

    def test_curated_summaries_reach_the_published_catalog(self):
        # Pins the wiring between the table and the emitted catalog.
        published = {
            operation["operation_id"]: operation["summary"]
            for operation in self.catalog["operations"]
        }
        for operation_id, summary in generate_api.CURATED_SUMMARIES.items():
            with self.subTest(operation=operation_id):
                self.assertEqual(published[operation_id], summary)

    def test_curation_is_confined_to_operations_upstream_left_ambiguous(self):
        # The table exists to resolve collisions, so an entry for an operation
        # whose upstream summary was already unique is a rewrite for its own
        # sake. This is the enforceable half of "says something upstream did
        # not": a machine can check that an operation needed disambiguating,
        # but not that the replacement wording is better than a paraphrase.
        specification = json.loads(generate_api.SOURCE.read_bytes())
        upstream = {
            operation["operationId"]: (operation.get("summary") or "")
            for item in specification["paths"].values()
            for method, operation in item.items()
            if method in generate_api.HTTP_METHODS and "operationId" in operation
        }
        exposed = {
            operation["operation_id"]
            for operation in self.catalog["operations"]
            if operation["exposed"]
        }
        families = {}
        for operation_id in exposed:
            families.setdefault(upstream[operation_id].strip().lower(), []).append(operation_id)
        ambiguous = {
            operation_id
            for members in families.values()
            if len(members) > 1
            for operation_id in members
        }
        gratuitous = sorted(set(generate_api.CURATED_SUMMARIES) - ambiguous)
        self.assertEqual(
            gratuitous, [], f"curated summaries for already-unambiguous operations: {gratuitous}"
        )

    def test_every_curated_summary_differs_from_the_text_it_replaces(self):
        # Read from the pinned specification rather than from the generated
        # catalog: comparing the catalog against the table that produced it
        # would restate the previous test. Each entry exists to say something
        # upstream did not, so one that reproduces the upstream wording is dead
        # weight implying a disambiguation it never makes — and uniqueness
        # alone would not catch it, because the colliding siblings are curated
        # too.
        specification = json.loads(generate_api.SOURCE.read_bytes())
        upstream = {
            operation["operationId"]: (operation.get("summary") or "")
            for item in specification["paths"].values()
            for method, operation in item.items()
            if method in generate_api.HTTP_METHODS and "operationId" in operation
        }
        for operation_id, summary in generate_api.CURATED_SUMMARIES.items():
            with self.subTest(operation=operation_id):
                self.assertNotEqual(summary.strip().lower(), upstream[operation_id].strip().lower())

    def test_catalog_is_exhaustive_and_stably_named(self):
        self.assertEqual(self.catalog["operation_count"], 471)
        self.assertEqual(self.catalog["exposed_operation_count"], 467)
        names = [
            operation["tool_name"] for operation in self.catalog["operations"]
        ]
        self.assertEqual(len(names), len(set(names)))
        self.assertEqual(
            self.operation("repoGet")["tool_name"], "repository.get"
        )
        self.assertEqual(
            self.operation("repoCreateKey")["tool_name"],
            "repository.create_deploy_key",
        )
        self.assertEqual(
            self.operation("repoUpdatePullRequest")["tool_name"],
            "repository.update_pull_request_branch",
        )

    def test_curated_guidance_covers_agent_search_vocabulary(self):
        expected_terms = {
            "repoGet": ("settings", "default branch", "permissions"),
            "repoCreateBranchProtection": (
                "merge guards",
                "required status checks",
                "approval",
            ),
            "repoCreateKey": ("SSH deploy key",),
            "ActionsDispatchWorkflow": ("run", "workflow", "ref"),
            "notifyReadList": ("mark", "read", "pinned"),
            "listPackages": ("container", "npm", "PyPI", "Cargo"),
        }
        for operation_id, terms in expected_terms.items():
            with self.subTest(operation_id=operation_id):
                guidance = self.operation(operation_id)["agent_guidance"]
                for term in terms:
                    self.assertIn(term.casefold(), guidance.casefold())

    def test_every_curated_name_replaces_its_generated_predecessor(self):
        published_names = {
            operation["tool_name"]
            for operation in self.catalog["operations"]
        }
        for operation_id, curated_name in generate_api.CURATED_TOOL_NAMES.items():
            with self.subTest(operation_id=operation_id):
                operation = self.operation(operation_id)
                generated_name = generate_api.generated_tool_name(
                    operation_id, operation["tag"]
                )
                self.assertNotEqual(curated_name, generated_name)
                self.assertEqual(operation["tool_name"], curated_name)
                self.assertNotIn(generated_name, published_names)

    def collect_vendor_extensions(self, value, found):
        if isinstance(value, dict):
            for name, nested in value.items():
                if name.startswith("x-"):
                    found.add(name)
                self.collect_vendor_extensions(nested, found)
        elif isinstance(value, list):
            for nested in value:
                self.collect_vendor_extensions(nested, found)
        return found

    def test_go_codegen_extensions_are_absent_from_the_agent_facing_catalog(self):
        # Schemas are what an agent reads to choose a tool and fill its
        # arguments. A Go struct field name and import path describe the
        # server's implementation, not its API, so they are cost without
        # meaning in every request that carries the tool list.
        found = set()
        for operation in self.catalog["operations"]:
            self.collect_vendor_extensions(operation["input_schema"], found)
        # Asserted against the literal keys rather than the configured set: an
        # emptied set would satisfy an intersection check vacuously, which is
        # the failure this test exists to catch.
        self.assertNotIn("x-go-name", found)
        self.assertNotIn("x-go-package", found)

    def test_parameter_metadata_drops_unknown_upstream_fields(self):
        # Exercised against a synthetic parameter rather than the catalog. The
        # pinned specification happens to carry no vendor keys on any parameter,
        # so scanning generated output would pass even if this allowlist were
        # replaced by a passthrough — it would assert a property of the input,
        # not of the function.
        metadata = generate_api.parameter_metadata(
            {
                "name": "owner",
                "in": "path",
                "required": True,
                "type": "string",
                "x-go-name": "Owner",
                "x-go-package": "code.gitea.io/gitea/modules/structs",
                "x-unknown-future-key": "whatever",
            }
        )
        self.assertEqual(
            metadata, {"name": "owner", "location": "path", "required": True, "type": "string"}
        )

    def test_enum_value_semantics_survive_extension_stripping(self):
        # x-go-enum-desc is the one member of this family that carries meaning:
        # it says what each enum value does. Stripping it to save bytes would
        # remove the reason a caller can choose between them.
        event = self.operation("repoCreatePullReview")["input_schema"]["definitions"][
            "CreatePullReviewOptions"
        ]["properties"]["event"]
        self.assertIn("APPROVED", event["enum"])
        self.assertIn("x-go-enum-desc", event)

    def test_input_schema_carries_only_reachable_definitions(self):
        operation = self.operation("repoCreateBranchProtection")
        schema = operation["input_schema"]
        self.assertEqual(schema["required"], ["owner", "repo"])
        self.assertIn("CreateBranchProtectionOption", schema["definitions"])
        self.assertNotIn("AccessToken", schema["definitions"])

    def test_nested_object_schemas_are_closed_without_closing_declared_maps(self):
        branch = self.operation("repoCreateBranch")
        branch_option = branch["input_schema"]["definitions"][
            "CreateBranchRepoOption"
        ]
        self.assertFalse(branch_option["additionalProperties"])

        hook = self.operation("repoCreateHook")
        hook_config = hook["input_schema"]["definitions"][
            "CreateHookOptionConfig"
        ]
        self.assertEqual(
            hook_config["additionalProperties"], {"type": "string"}
        )

    def test_every_generated_input_object_has_an_explicit_unknown_field_policy(self):
        for operation in self.catalog["operations"]:
            with self.subTest(operation_id=operation["operation_id"]):
                self.assert_object_schemas_closed(operation["input_schema"])

    def test_file_parameters_have_typed_mcp_transport_shape(self):
        operation = self.operation("issueCreateIssueAttachment")
        attachment = operation["input_schema"]["properties"]["attachment"]
        self.assertEqual(
            attachment["required"], ["filename", "content_base64"]
        )
        self.assertFalse(attachment["additionalProperties"])

    def test_non_service_auth_is_never_silently_exposed(self):
        token = self.operation("userCreateToken")
        self.assertEqual(token["auth_lane"], "basic")
        self.assertFalse(token["exposed"])
        self.assertIn(
            "userCreateToken",
            {
                entry["operation_id"]
                for entry in self.catalog["compatibility_ledger"]
            },
        )
        replacement = next(
            entry["replacement"]
            for entry in self.catalog["compatibility_ledger"]
            if entry["operation_id"] == "userCreateToken"
        )
        self.assertEqual(replacement, "access_token.create")

    def test_read_only_post_operations_are_not_annotated_as_mutations(self):
        for operation_id in generate_api.READ_ONLY_POST_OPERATIONS:
            with self.subTest(operation_id=operation_id):
                operation = self.operation(operation_id)
                self.assertEqual(operation["method"], "POST")
                self.assertEqual(operation["risk"], "read")

    def test_credential_bearing_results_are_classified_as_sensitive(self):
        for operation_id in generate_api.SECRET_RESULT_OPERATIONS:
            with self.subTest(operation_id=operation_id):
                self.assertTrue(self.operation(operation_id)["secret_result"])

        self.assertTrue(self.operation("adminListHooks")["secret_result"])
        self.assertFalse(self.operation("repoGet")["secret_result"])

    def test_declared_success_headers_are_preserved(self):
        token = self.operation("adminCreateRunnerRegistrationToken")
        self.assertEqual(token["response_headers"], ["token"])

    def test_non_delete_irreversible_operations_are_destructive(self):
        self.assertEqual(
            self.operation("repoMergePullRequest")["risk"], "destructive"
        )
        self.assertEqual(self.operation("repoTransfer")["risk"], "destructive")
        self.assertEqual(self.operation("repoEdit")["risk"], "destructive")
        self.assertEqual(self.operation("repoCreateBranch")["risk"], "mutation")
        self.assertEqual(self.operation("userCurrentPutStar")["risk"], "mutation")
        self.assertEqual(self.operation("orgAddTeamMember")["risk"], "mutation")
        self.assertTrue(self.operation("repoMigrate")["open_world"])
        self.assertTrue(self.operation("repoTestHook")["open_world"])
        self.assertFalse(self.operation("repoGet")["open_world"])


if __name__ == "__main__":
    unittest.main()
