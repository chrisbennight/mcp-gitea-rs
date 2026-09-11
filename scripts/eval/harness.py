"""Agent-task evaluation harness for the model-visible surface.

The surface is a contract with a non-deterministic consumer, so changes to it
are justified by measured task outcomes rather than inspection (DECISIONS.md,
"Tool-surface changes are evaluation-gated"). This harness is the instrument:
it drives a headless agent through outcome-phrased tasks against a disposable
Gitea instance and this repository's server, verifies each outcome directly
against the Gitea API, and reports task success, tool-call count, token cost,
and tool-error rate.

Run through ``run_eval.sh``, which owns provisioning and teardown; this module
assumes a reachable Gitea, a reachable MCP server, and a `claude` CLI, all
named by environment variables. It never contacts live infrastructure: every
URL it touches is the disposable instance the wrapper started.

Credentials arrive by environment variable and are written only to the
scratch MCP configuration the agent needs; they are never printed.
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import os
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from collections.abc import Callable

#: Turn ceiling per task. A task that cannot finish inside it is failed with a
#: visible reason rather than left running; the ceiling bounds cost, not
#: ambition — the hardest task here needs far fewer turns.
MAX_TURNS = 40

#: Wall-clock ceiling per task, seconds.
TASK_TIMEOUT_SECONDS = 900

#: The surface this server serves, recorded so a report can be compared
#: against the baselines taken while both surfaces existed.
SURFACE = "layered"

#: The owner every task works under. Created by the wrapper.
OWNER = "scope-admin"

#: The secondary account some tasks reference. Created by the wrapper.
COLLABORATOR = "scope-collaborator"


class TaskSkipped(Exception):
    """Raised by a seed to declare an unmet precondition.

    A skip is not a pass: the report separates the three outcomes so a
    hollowed-out run cannot masquerade as a green one."""


#: Gitea access tokens are forty hex characters. Anything of that shape is
#: masked before a transcript is retained: a one-time token value must never
#: survive in telemetry, and over-redacting same-shaped non-secrets such as
#: commit ids costs the metrics nothing.
TOKEN_SHAPE = re.compile(r"\b[0-9a-f]{40}\b")


def redact(text: str) -> str:
    return TOKEN_SHAPE.sub("[REDACTED-TOKEN-SHAPE]", text)


class GiteaApi:
    """Minimal Gitea REST client for seeding and verification.

    Ordinary calls authenticate with the admin token. The token-metadata
    endpoint is the one exception: Gitea serves it only under Basic
    authentication, the same upstream boundary that gives the server its
    separate credential lane, so the client mirrors that with `basic=True`.
    """

    def __init__(self, base_url: str, token: str, basic_credentials: str | None = None) -> None:
        self.base_url = base_url.rstrip("/")
        self.token = token
        self.basic_credentials = basic_credentials

    def request(
        self,
        method: str,
        path: str,
        body: dict | None = None,
        ok_missing: bool = False,
        basic: bool = False,
    ) -> dict | list | None:
        if basic:
            if not self.basic_credentials:
                raise RuntimeError("basic credentials are not configured")
            import base64 as _base64

            authorization = "Basic " + _base64.b64encode(self.basic_credentials.encode()).decode()
        else:
            authorization = f"token {self.token}"
        request = urllib.request.Request(
            f"{self.base_url}/api/v1{path}",
            method=method,
            data=None if body is None else json.dumps(body).encode(),
            headers={
                "Authorization": authorization,
                "Content-Type": "application/json",
            },
        )
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                raw = response.read()
        except urllib.error.HTTPError as error:
            if ok_missing and error.code == 404:
                return None
            detail = error.read().decode(errors="replace")[:500]
            raise RuntimeError(f"{method} {path}: HTTP {error.code}: {detail}") from error
        if not raw:
            return None
        return json.loads(raw)

    def get(self, path: str, ok_missing: bool = False) -> dict | list | None:
        return self.request("GET", path, ok_missing=ok_missing)

    def post(self, path: str, body: dict | None = None) -> dict | list | None:
        return self.request("POST", path, body)


@dataclasses.dataclass
class Task:
    """One outcome-phrased agent task with its seed and verifier."""

    id: str
    workload: str
    prompt: str
    seed: Callable[[GiteaApi], None]
    verify: Callable[[GiteaApi], tuple[bool, str]]


def _seed_repo(api: GiteaApi, name: str, files: dict[str, str] | None = None) -> None:
    api.post(
        "/user/repos",
        {"name": name, "private": True, "auto_init": True, "default_branch": "main"},
    )
    for path, content in (files or {}).items():
        import base64

        api.post(
            f"/repos/{OWNER}/{name}/contents/{path}",
            {
                "content": base64.b64encode(content.encode()).decode(),
                "message": f"seed {path}",
                "branch": "main",
            },
        )


def _verify_true(condition: bool, detail: str) -> tuple[bool, str]:
    return (True, "ok") if condition else (False, detail)


def _owner_repositories(api: GiteaApi) -> list[str]:
    return [
        repository["name"]
        for repository in (api.get(f"/users/{OWNER}/repos?limit=50") or [])
    ]


def snapshot_topics(api: GiteaApi) -> dict[str, tuple]:
    """Every owner repository's topic set plus the mutable summary the repo
    listing already carries, so preserved-state equality also sees visibility,
    description, default-branch, archive, and content changes (updated_at
    moves on a push) without extra calls."""
    listing = api.get(f"/users/{OWNER}/repos?limit=50") or []
    state: dict[str, tuple] = {}
    for repository in listing:
        name = repository["name"]
        topics = frozenset(
            (api.get(f"/repos/{OWNER}/{name}/topics") or {}).get("topics", [])
        )
        state[name] = (
            topics,
            repository.get("private"),
            repository.get("description"),
            repository.get("default_branch"),
            repository.get("archived"),
            repository.get("updated_at"),
        )
    return state


def snapshot_issues(api: GiteaApi) -> dict[tuple[str, int], tuple]:
    """Every issue and pull thread under the owner, reduced to the mutable
    fields a "leave everything else untouched" task must preserve.

    Listed as two queries because the pinned specification's `type` parameter
    admits only `issues` or `pulls`, not a combined form."""
    state: dict[tuple[str, int], tuple] = {}
    for name in _owner_repositories(api):
        for kind in ("issues", "pulls"):
            listing = (
                api.get(f"/repos/{OWNER}/{name}/issues?state=all&type={kind}&limit=50") or []
            )
            for issue in listing:
                labels = tuple(
                    sorted(label.get("name", "") for label in issue.get("labels") or [])
                )
                assignees = tuple(
                    sorted(user.get("login", "") for user in issue.get("assignees") or [])
                )
                milestone = (issue.get("milestone") or {}).get("title")
                # updated_at moves on any touch of the thread — a comment
                # edit included — so equality here covers mutations the named
                # fields cannot see.
                state[(name, issue["number"])] = (
                    issue.get("state"),
                    issue.get("title"),
                    issue.get("body"),
                    labels,
                    assignees,
                    milestone,
                    issue.get("due_date"),
                    issue.get("is_locked"),
                    issue.get("comments"),
                    issue.get("updated_at"),
                )
    return state


def _task_repository_lifecycle() -> Task:
    def seed(api: GiteaApi) -> None:
        pass

    def verify(api: GiteaApi) -> tuple[bool, str]:
        repo = api.get(f"/repos/{OWNER}/eval-lifecycle", ok_missing=True)
        if repo is None:
            return False, "repository eval-lifecycle does not exist"
        if not repo.get("private"):
            return False, "repository is not private"
        labels = api.get(f"/repos/{OWNER}/eval-lifecycle/labels") or []
        if not any(label.get("name") == "bug" for label in labels):
            return False, "label bug is missing"
        branches = api.get(f"/repos/{OWNER}/eval-lifecycle/branches") or []
        if not branches:
            return False, "repository has no initialized branch"
        topics = (api.get(f"/repos/{OWNER}/eval-lifecycle/topics") or {}).get("topics", [])
        return _verify_true("automation" in topics, "topic automation is missing")

    return Task(
        id="repository-lifecycle",
        workload="repository lifecycle",
        prompt=(
            f"Using the gitea tools, create a private repository named eval-lifecycle "
            f"under the user {OWNER}, initialized with a default branch. Give it an "
            f"issue label named 'bug' (any color) and the repository topic "
            f"'automation'. Report what you created."
        ),
        seed=seed,
        verify=verify,
    )


#: A marker only visible in the seeded pull request's changed content. The
#: change-review verifier requires the review comment to quote it, so a pass
#: proves the agent retrieved the change through the surface rather than
#: paraphrasing the pull request title.
REVIEW_MARKER = "salmagundi-7c41"


def _task_change_review() -> Task:
    def seed(api: GiteaApi) -> None:
        _seed_repo(api, "eval-review", {"greeting.txt": "hello\n"})
        api.post(
            f"/repos/{OWNER}/eval-review/branches",
            {"new_branch_name": "feature", "old_branch_name": "main"},
        )
        import base64

        contents = api.get(f"/repos/{OWNER}/eval-review/contents/greeting.txt?ref=feature")
        api.request(
            "PUT",
            f"/repos/{OWNER}/eval-review/contents/greeting.txt",
            {
                "content": base64.b64encode(
                    f"hello, {REVIEW_MARKER}\n".encode()
                ).decode(),
                "message": "update greeting",
                "branch": "feature",
                "sha": contents["sha"],
            },
        )
        api.post(
            f"/repos/{OWNER}/eval-review/pulls",
            {"title": "Update greeting", "head": "feature", "base": "main"},
        )

    def verify(api: GiteaApi) -> tuple[bool, str]:
        pull = api.get(f"/repos/{OWNER}/eval-review/pulls/1")
        if not pull.get("merged"):
            return False, "pull request 1 is not merged"
        comments = api.get(f"/repos/{OWNER}/eval-review/issues/1/comments") or []
        marker_comment = next(
            (
                comment
                for comment in comments
                if REVIEW_MARKER in (comment.get("body") or "")
            ),
            None,
        )
        if marker_comment is None:
            return False, "no comment quotes the changed content, so the diff was never read"
        # Review precedes merge: a summary posted after merging reviewed
        # nothing the merge decision could use. The edit timestamp is held to
        # the same bound, or a pre-merge comment could gain its quote after
        # the fact while keeping its original creation time.
        merged_at = pull.get("merged_at") or ""
        created_at = marker_comment.get("created_at") or ""
        updated_at = marker_comment.get("updated_at") or created_at
        settled = max(created_at, updated_at)
        # Strictly before: with second-granularity timestamps an equal value
        # cannot distinguish review-then-merge from its reversal, so equality
        # does not count as evidence.
        return _verify_true(
            bool(merged_at) and bool(settled) and settled < merged_at,
            "the diff-quoting review comment did not exist unedited before the merge",
        )

    return Task(
        id="change-review",
        workload="change review",
        prompt=(
            f"Repository {OWNER}/eval-review has one open pull request. Using the "
            f"gitea tools, read the pull request's diff, post a comment on it that "
            f"quotes the exact new greeting value the change introduces, then merge "
            f"it."
        ),
        seed=seed,
        verify=verify,
    )


def _task_publication() -> Task:
    def seed(api: GiteaApi) -> None:
        _seed_repo(api, "eval-release")

    def verify(api: GiteaApi) -> tuple[bool, str]:
        release = api.get(f"/repos/{OWNER}/eval-release/releases/tags/v1.0.0", ok_missing=True)
        if release is None:
            return False, "release v1.0.0 does not exist"
        if release.get("target_commitish") not in ("main", ""):
            return False, f"release targets {release.get('target_commitish')}, not main"
        if release.get("name") != "First release":
            return False, f"release is titled {release.get('name')!r}, not 'First release'"
        if release.get("draft"):
            return False, "the release is a draft, not published"
        return _verify_true(bool(release.get("body")), "release has no notes")

    return Task(
        id="publication",
        workload="publication",
        prompt=(
            f"Using the gitea tools, publish a release for {OWNER}/eval-release: "
            f"tag v1.0.0 on the main branch, release titled 'First release', with a "
            f"short body describing it as the first release."
        ),
        seed=seed,
        verify=verify,
    )


def _task_access_administration() -> Task:
    def seed(api: GiteaApi) -> None:
        pass

    def verify(api: GiteaApi) -> tuple[bool, str]:
        org = api.get("/orgs/eval-org", ok_missing=True)
        if org is None:
            return False, "organization eval-org does not exist"
        teams = api.get("/orgs/eval-org/teams") or []
        devs = next((team for team in teams if team.get("name") == "devs"), None)
        if devs is None:
            return False, "team devs does not exist"
        # Gitea reports the top-level permission as "none" when unit-scoped
        # permissions carry the levels, so write access can live in either
        # field; requiring the top-level field alone fails correct teams.
        units = devs.get("units_map") or {}
        if devs.get("permission") != "write" and "write" not in units.values():
            grants = devs.get("permission"), sorted(set(units.values()))
            return False, f"team devs grants {grants}, not write"
        members = api.get(f"/teams/{devs['id']}/members") or []
        return _verify_true(
            any(member.get("login") == COLLABORATOR for member in members),
            f"{COLLABORATOR} is not a member of devs",
        )

    return Task(
        id="access-administration",
        workload="access and identity administration",
        prompt=(
            f"Using the gitea tools, create an organization named eval-org, create a "
            f"team in it named devs with write permission, and add the user "
            f"{COLLABORATOR} to that team."
        ),
        seed=seed,
        verify=verify,
    )


def _task_fleet_query() -> Task:
    before: dict[str, tuple] = {}
    heads: dict[str, str] = {}

    def seed(api: GiteaApi) -> None:
        for name, topics in [
            ("eval-fleet-a", ["homelab"]),
            ("eval-fleet-b", ["homelab"]),
            ("eval-fleet-c", ["other"]),
        ]:
            _seed_repo(api, name)
            api.request("PUT", f"/repos/{OWNER}/{name}/topics", {"topics": topics})
        before.update(snapshot_topics(api))
        # The matched repositories' update stamps legitimately move with the
        # topic change, so the stamp cannot distinguish topic-only from
        # topic-plus-content there; their branch heads can.
        for name in ("eval-fleet-a", "eval-fleet-b"):
            branch = api.get(f"/repos/{OWNER}/{name}/branches/main") or {}
            heads[name] = ((branch.get("commit") or {}).get("id")) or ""

    def verify(api: GiteaApi) -> tuple[bool, str]:
        # Exact sets on the seeded fleet: the task demands an additive change
        # on the matches and no change elsewhere, so replacing the seeded
        # topic or touching the non-matching repository must fail, not merely
        # "audited missing".
        for name, expected in [
            ("eval-fleet-a", {"homelab", "audited"}),
            ("eval-fleet-b", {"homelab", "audited"}),
            ("eval-fleet-c", {"other"}),
        ]:
            topics = set((api.get(f"/repos/{OWNER}/{name}/topics") or {}).get("topics", []))
            if topics != expected:
                return False, f"{name} topics are {sorted(topics)}, expected {sorted(expected)}"
        # "Only those repositories" spans the whole owner, and other tasks'
        # repositories share the instance. Symmetric equality is the whole
        # contract: the after-state must equal the before-state with exactly
        # the requested delta applied, so a deleted repository, a created one,
        # and any other topic change all fail — not only the specific
        # mutation the task performs.
        expected = dict(before)
        for name in ("eval-fleet-a", "eval-fleet-b"):
            topics, *rest = expected[name]
            expected[name] = (topics | {"audited"}, *rest)
        after = snapshot_topics(api)
        # Adding a topic bumps the repository's own update stamp, which is
        # part of the requested change rather than drift — but ONLY the stamp
        # is absorbed. Every other field of the matched repositories must
        # still equal its snapshot, or a mutation beyond the requested topic
        # would ride along unnoticed.
        for name in ("eval-fleet-a", "eval-fleet-b"):
            observed = after.get(name)
            expected_topics, *expected_rest = expected[name]
            if observed and observed[0] == expected_topics:
                expected[name] = (expected_topics, *expected_rest[:-1], observed[-1])
            branch = api.get(f"/repos/{OWNER}/{name}/branches/main") or {}
            head = ((branch.get("commit") or {}).get("id")) or ""
            if head != heads.get(name):
                return False, f"{name} received a content change beyond the requested topic"
        if after != expected:
            drift = sorted(
                set(after.items()) ^ set(expected.items()),
                key=lambda item: item[0],
            )
            return False, f"owner repository state drifted beyond the requested change: {drift[:4]}"
        return True, "ok"

    return Task(
        id="fleet-query",
        workload="fleet queries",
        prompt=(
            f"Using the gitea tools, find every repository under {OWNER} that carries "
            f"the topic 'homelab', and add the topic 'audited' to each of those "
            f"repositories only. Repositories without the homelab topic must not be "
            f"changed. Report which repositories you changed."
        ),
        seed=seed,
        verify=verify,
    )


def _task_triage() -> Task:
    before: dict[tuple[str, int], tuple] = {}

    def seed(api: GiteaApi) -> None:
        _seed_repo(api, "eval-triage")
        bug = api.post(
            f"/repos/{OWNER}/eval-triage/labels",
            {"name": "bug", "color": "#ff0000"},
        )
        api.post(
            f"/repos/{OWNER}/eval-triage/issues",
            {"title": "Crash when saving", "body": "boom", "labels": [bug["id"]]},
        )
        api.post(
            f"/repos/{OWNER}/eval-triage/issues",
            {"title": "Feature wish", "body": "please"},
        )
        before.update(snapshot_issues(api))

    def verify(api: GiteaApi) -> tuple[bool, str]:
        issue = api.get(f"/repos/{OWNER}/eval-triage/issues/1")
        if issue.get("state") != "closed":
            return False, "the bug issue is not closed"
        # "Untouched everywhere" spans every issue and pull thread under the
        # owner. Symmetric equality with the target excluded from both sides:
        # a modified, deleted, or newly created thread anywhere else fails.
        after = snapshot_issues(api)
        target = ("eval-triage", 1)
        rest_after = {key: value for key, value in after.items() if key != target}
        rest_before = {key: value for key, value in before.items() if key != target}
        if rest_after != rest_before:
            drifted = sorted(set(rest_after) ^ set(rest_before)) or sorted(
                key for key in rest_after if rest_after[key] != rest_before.get(key)
            )
            return False, f"non-target threads changed: {drifted[:4]}"
        comments = api.get(f"/repos/{OWNER}/eval-triage/issues/1/comments") or []
        return _verify_true(
            any("triag" in (comment.get("body") or "").lower() for comment in comments),
            "no closing comment explains the triage",
        )

    return Task(
        id="triage",
        workload="triage",
        prompt=(
            f"Somewhere in the repositories under {OWNER} there is exactly one open "
            f"issue labeled 'bug'. Using the gitea search and issue tools, find it "
            f"without being told which repository it is in, close it with a comment "
            f"explaining that it is being closed as triaged, and leave every other "
            f"issue everywhere untouched."
        ),
        seed=seed,
        verify=verify,
    )


def _task_content_read() -> Task:
    def seed(api: GiteaApi) -> None:
        _seed_repo(
            api,
            "eval-content",
            {"config.toml": '[server]\nport = 4821\nname = "eval"\n'},
        )

    def verify(api: GiteaApi) -> tuple[bool, str]:
        issues = api.get(f"/repos/{OWNER}/eval-content/issues?state=all") or []
        issue = next(
            (candidate for candidate in issues if candidate.get("title") == "Configured port"),
            None,
        )
        if issue is None:
            return False, "issue 'Configured port' does not exist"
        return _verify_true(
            "4821" in (issue.get("body") or ""),
            "the issue body does not quote the configured port",
        )

    return Task(
        id="content-read",
        workload="content read",
        prompt=(
            f"Using the gitea tools, read the file config.toml on the main branch of "
            f"{OWNER}/eval-content and open an issue in that repository titled "
            f"'Configured port' whose body states the port number the file "
            f"configures."
        ),
        seed=seed,
        verify=verify,
    )


def _task_issue_thread() -> Task:
    def seed(api: GiteaApi) -> None:
        _seed_repo(api, "eval-issues")

    def verify(api: GiteaApi) -> tuple[bool, str]:
        issues = api.get(f"/repos/{OWNER}/eval-issues/issues?state=open") or []
        issue = next(
            (candidate for candidate in issues if candidate.get("title") == "Track rollout"),
            None,
        )
        if issue is None:
            return False, "issue 'Track rollout' does not exist"
        labels = [label.get("name") for label in issue.get("labels") or []]
        if "rollout" not in labels:
            return False, "label rollout is missing from the issue"
        comments = api.get(f"/repos/{OWNER}/eval-issues/issues/{issue['number']}/comments") or []
        return _verify_true(
            any("track" in (comment.get("body") or "").lower() for comment in comments),
            "no status comment says the rollout is being tracked",
        )

    return Task(
        id="issue-thread",
        workload="repository lifecycle",
        prompt=(
            f"Using the gitea tools, open an issue titled 'Track rollout' in "
            f"{OWNER}/eval-issues, create a label named 'rollout' in that repository "
            f"and apply it to the issue, then add a first status comment saying the "
            f"rollout is being tracked."
        ),
        seed=seed,
        verify=verify,
    )


def _task_actions_visibility() -> Task:
    workflow = """name: eval-nightly
on: workflow_dispatch
jobs:
  build:
    runs-on: linux
    steps:
      - run: 'true'
"""

    def seed(api: GiteaApi) -> None:
        _seed_repo(api, "eval-actions", {".gitea/workflows/eval-nightly.yml": workflow})
        # Actions starts disabled, so the requested enablement is a real
        # mutation the agent must perform rather than a property the seed
        # already satisfied.
        api.request("PATCH", f"/repos/{OWNER}/eval-actions", {"has_actions": False})

    def verify(api: GiteaApi) -> tuple[bool, str]:
        repo = api.get(f"/repos/{OWNER}/eval-actions")
        if not repo.get("has_actions"):
            return False, "Actions is not enabled"
        variable = api.get(
            f"/repos/{OWNER}/eval-actions/actions/variables/NIGHTLY_CHANNEL", ok_missing=True
        )
        if variable is None or variable.get("data") != "stable":
            return False, "Actions variable NIGHTLY_CHANNEL is not set to stable"
        runs = api.get(f"/repos/{OWNER}/eval-actions/actions/runs", ok_missing=True) or {}
        if (runs.get("total_count") or 0) > 0 or runs.get("workflow_runs"):
            return False, "a workflow run was queued despite the no-dispatch instruction"
        issues = api.get(f"/repos/{OWNER}/eval-actions/issues?state=all") or []
        issue = next(
            (
                candidate
                for candidate in issues
                if candidate.get("title") == "Workflow inventory"
            ),
            None,
        )
        if issue is None:
            return False, "issue 'Workflow inventory' does not exist"
        return _verify_true(
            "eval-nightly" in (issue.get("body") or ""),
            "the inventory does not name the workflow the repository defines",
        )

    return Task(
        id="ci-operation",
        workload="CI operation",
        prompt=(
            f"The repository {OWNER}/eval-actions defines a workflow but has its "
            f"Actions unit disabled. Using the gitea tools, enable Actions on that "
            f"repository, create an Actions repository variable named "
            f"NIGHTLY_CHANNEL with the value stable, then read the repository's "
            f"workflow definitions through the Actions tools and open an issue "
            f"titled 'Workflow inventory' whose body names each workflow the "
            f"repository defines. This instance has no Actions runner, so do not "
            f"try to dispatch or wait for a run."
        ),
        seed=seed,
        verify=verify,
    )


def _task_token_lifecycle() -> Task:
    def seed(api: GiteaApi) -> None:
        pass

    def verify(api: GiteaApi) -> tuple[bool, str]:
        tokens = api.request("GET", f"/users/{OWNER}/tokens", basic=True) or []
        token = next((entry for entry in tokens if entry.get("name") == "eval-scoped"), None)
        if token is None:
            return False, "token eval-scoped does not exist"
        scopes = token.get("scopes") or []
        return _verify_true(
            scopes == ["read:repository"],
            f"token scopes {scopes} are not exactly read:repository",
        )

    return Task(
        id="token-lifecycle",
        workload="access and identity administration",
        prompt=(
            "Using the gitea tools, create a scoped access token named eval-scoped "
            "with read-only repository scope. Do not repeat the token value in "
            "your final answer."
        ),
        seed=seed,
        verify=verify,
    )


def tasks() -> list[Task]:
    return [
        _task_repository_lifecycle(),
        _task_change_review(),
        _task_publication(),
        _task_access_administration(),
        _task_fleet_query(),
        _task_triage(),
        _task_content_read(),
        _task_issue_thread(),
        _task_actions_visibility(),
        _task_token_lifecycle(),
    ]


def transcript_metrics(lines: list[str]) -> dict:
    """Fold a claude stream-json transcript into the reported metrics.

    Pure over its input so it can be unit-tested without an agent. Unknown
    event shapes are ignored rather than fatal: the metrics are a report, and
    a transcript that parses partially still says more than a crash.
    """
    tool_calls = 0
    tool_errors = 0
    tool_names: dict[str, int] = {}
    usage: dict[str, int] = {}
    running_usage: dict[str, int] = {}
    num_turns = None
    cost_usd = None
    duration_ms = None
    model = None
    result_seen = False
    for line in lines:
        line = line.strip()
        if not line:
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(event, dict):
            continue
        message = event.get("message")
        if event.get("type") == "assistant" and isinstance(message, dict):
            if model is None:
                model = message.get("model")
            # Accumulated per-message usage stands in for the final total when
            # the stream dies before its result event, so an interrupted run
            # still accounts the tokens it already spent.
            for key, value in (message.get("usage") or {}).items():
                if isinstance(value, int):
                    running_usage[key] = running_usage.get(key, 0) + value
            for block in message.get("content") or []:
                if isinstance(block, dict) and block.get("type") == "tool_use":
                    tool_calls += 1
                    name = str(block.get("name"))
                    tool_names[name] = tool_names.get(name, 0) + 1
        if event.get("type") == "user" and isinstance(message, dict):
            for block in message.get("content") or []:
                if (
                    isinstance(block, dict)
                    and block.get("type") == "tool_result"
                    and block.get("is_error")
                ):
                    tool_errors += 1
        if event.get("type") == "result":
            # A result event only completes the accounting when it actually
            # accounts: a hollow final event must not launder zeroed metrics
            # into a comparison artifact.
            result_seen = (
                bool(event.get("usage"))
                and event.get("num_turns") is not None
                and event.get("total_cost_usd") is not None
            )
            usage = event.get("usage") or {}
            num_turns = event.get("num_turns")
            cost_usd = event.get("total_cost_usd")
            duration_ms = event.get("duration_ms")
    return {
        "tool_calls": tool_calls,
        "tool_errors": tool_errors,
        "tool_names": tool_names,
        "usage": usage or running_usage,
        "num_turns": num_turns,
        "cost_usd": cost_usd,
        "duration_ms": duration_ms,
        "model": model,
        "result_seen": result_seen,
    }


def run_agent(
    prompt: str, mcp_config: str, transcript_path: pathlib.Path, model: str | None = None
) -> dict:
    """Run one headless agent task and return its transcript metrics."""
    command = [
        "claude",
        "-p",
        prompt,
        *(["--model", model] if model else []),
        "--output-format",
        "stream-json",
        "--verbose",
        "--max-turns",
        str(MAX_TURNS),
        "--mcp-config",
        mcp_config,
        "--strict-mcp-config",
        # An allowlist, not a bypass: only the surface under measurement is
        # approved, and in headless mode every other tool is denied, so the
        # agent cannot reach the upstream around the MCP server.
        "--allowedTools",
        "mcp__gitea__*",
        # An allowlist alone does not override ambient user settings that
        # pre-approve built-ins, so every outcome-capable built-in is denied
        # explicitly — an explicit deny wins over any ambient allow. The
        # client's schema loader stays available: with a catalog this size the
        # CLI defers tool schemas behind its search tool, and denying it would
        # sever access to the very surface under measurement. It loads
        # definitions; it cannot touch the upstream.
        "--disallowedTools",
        "Bash,Edit,Write,NotebookEdit,WebFetch,WebSearch,Task,Agent,Skill,SlashCommand,"
        "Read,Glob,Grep,NotebookRead,LS",
    ]
    # The agent must not inherit the harness's upstream credentials, or the
    # measurement stops being of the MCP surface at all.
    environment = {
        key: value for key, value in os.environ.items() if not key.startswith("GITEA_EVAL_")
    }
    with (
        tempfile.TemporaryDirectory(prefix="gitea-eval-agent-") as workdir,
        tempfile.TemporaryDirectory(prefix="gitea-eval-config-") as config_home,
    ):
        # An isolated configuration home, seeded with the auth credential
        # only, then deleted with the run. Two boundaries depend on it: the
        # CLI's own session persistence records raw tool results — a minted
        # token included — and must not outlive the run or land outside it;
        # and the caller's settings, memory, plugins, and hooks must not
        # steer a measurement they are not part of. It lives OUTSIDE the
        # agent's working directory, and the filesystem read tools are denied
        # besides, so the measured model has no path to the credential it is
        # authenticated with.
        config_dir = pathlib.Path(config_home)
        source_credentials = pathlib.Path.home() / ".claude" / ".credentials.json"
        if source_credentials.exists():
            shutil.copy(source_credentials, config_dir / ".credentials.json")
        environment["CLAUDE_CONFIG_DIR"] = str(config_dir)
        try:
            completed = subprocess.run(
                command,
                cwd=workdir,
                env=environment,
                capture_output=True,
                text=True,
                timeout=TASK_TIMEOUT_SECONDS,
                check=False,
            )
        except subprocess.TimeoutExpired as expiry:
            # The partial stream is telemetry too; losing it would make the
            # one class of failure that most needs diagnosis the only one
            # without a transcript, and its metrics still count the work and
            # cost the run already spent.
            partial = expiry.stdout or b""
            if isinstance(partial, bytes):
                partial = partial.decode(errors="replace")
            transcript_path.write_text(redact(partial))
            metrics = transcript_metrics(partial.splitlines())
            metrics.pop("result_seen", None)
            metrics["agent_error"] = f"timed out after {TASK_TIMEOUT_SECONDS}s"
            return metrics
    transcript_path.write_text(redact(completed.stdout))
    metrics = transcript_metrics(completed.stdout.splitlines())
    if completed.returncode != 0:
        metrics["agent_error"] = f"claude exited {completed.returncode}"
    elif not metrics.pop("result_seen"):
        # A truncated or reshaped stream must not pass with zeroed metrics:
        # the comparison instrument would silently flatter whatever run lost
        # its accounting.
        metrics["agent_error"] = "transcript carried no result event"
    else:
        metrics.pop("result_seen", None)
    return metrics


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, help="report JSON path")
    parser.add_argument(
        "--transcripts", required=True, help="directory that receives one transcript per task"
    )
    parser.add_argument(
        "--only",
        default=None,
        help="comma-separated task ids to run; default runs the whole set",
    )
    parser.add_argument(
        "--model",
        default=None,
        help="model the agent runs; pin it so surface comparisons share one",
    )
    arguments = parser.parse_args()

    gitea_url = os.environ["GITEA_EVAL_URL"]
    admin_token = os.environ["GITEA_EVAL_ADMIN_TOKEN"]
    mcp_url = os.environ["GITEA_EVAL_MCP_URL"]
    mcp_bearer = os.environ["GITEA_EVAL_MCP_BEARER"]

    transcripts = pathlib.Path(arguments.transcripts)
    transcripts.mkdir(parents=True, exist_ok=True)
    api = GiteaApi(
        gitea_url,
        admin_token,
        basic_credentials=os.environ.get("GITEA_EVAL_ADMIN_BASIC"),
    )

    mcp_config_path = transcripts / "mcp-config.json"
    mcp_config_path.write_text(
        json.dumps(
            {
                "mcpServers": {
                    "gitea": {
                        "type": "http",
                        "url": mcp_url,
                        "headers": {"Authorization": f"Bearer {mcp_bearer}"},
                    }
                }
            }
        )
    )

    selected = tasks()
    if arguments.only:
        wanted = set(arguments.only.split(","))
        selected = [task for task in selected if task.id in wanted]
        missing = wanted - {task.id for task in selected}
        if missing:
            print(f"unknown task ids: {sorted(missing)}", file=sys.stderr)
            return 2

    results = []
    for task in selected:
        print(f"[{task.id}] seeding", flush=True)
        # A crashing seed or verifier fails its own task with a visible
        # reason; it must never abort the run and discard the report the
        # remaining tasks earned.
        try:
            task.seed(api)
        except TaskSkipped as reason:
            print(f"[{task.id}] skipped: {reason}", flush=True)
            results.append(
                {
                    "task": task.id,
                    "workload": task.workload,
                    "outcome": "skipped",
                    "success": False,
                    "detail": str(reason),
                    "wall_seconds": 0.0,
                }
            )
            continue
        except Exception as error:  # noqa: BLE001 - reported, not swallowed
            print(f"[{task.id}] FAIL: seed crashed: {error}", flush=True)
            results.append(
                {
                    "task": task.id,
                    "workload": task.workload,
                    "outcome": "failed",
                    "success": False,
                    "detail": f"seed crashed: {error}",
                    "wall_seconds": 0.0,
                }
            )
            continue
        print(f"[{task.id}] running agent", flush=True)
        started = time.monotonic()
        metrics = run_agent(
            task.prompt,
            str(mcp_config_path),
            transcripts / f"{task.id}.jsonl",
            model=arguments.model,
        )
        elapsed = round(time.monotonic() - started, 1)
        if "agent_error" in metrics:
            success, detail = False, metrics["agent_error"]
        else:
            try:
                success, detail = task.verify(api)
            except Exception as error:  # noqa: BLE001 - reported, not swallowed
                success, detail = False, f"verifier crashed: {error}"
            if success and not metrics.get("tool_calls"):
                # Every task's outcome requires tool mutations, so a verified
                # outcome with a transcript recording no tool use means the
                # stream's event shape drifted past the parser. Failing the
                # task keeps the instrument honest: corrupted comparison
                # metrics are worse than a visible parser regression.
                success = False
                detail = (
                    "outcome verified but the transcript recorded no tool calls; "
                    "transcript parsing no longer matches the CLI stream"
                )
        print(f"[{task.id}] {'pass' if success else 'FAIL'}: {detail} ({elapsed}s)", flush=True)
        results.append(
            {
                "task": task.id,
                "workload": task.workload,
                "outcome": "passed" if success else "failed",
                "success": success,
                "detail": detail,
                "wall_seconds": elapsed,
                **metrics,
            }
        )

    # One surface is served, so the label is a constant rather than a
    # choice a report could get wrong.
    report = aggregate_report(SURFACE, results)
    # The instrument itself is part of the comparison: reports from different
    # task definitions must not read as the same experiment, so the report
    # carries this file's digest — any change to a prompt, seed, verifier, or
    # the parser changes it.
    import hashlib

    report["harness_sha256"] = hashlib.sha256(
        pathlib.Path(__file__).read_bytes()
    ).hexdigest()
    # The client is part of the instrument: comparisons are honest only when
    # the CLI that drove both runs is known.
    try:
        report["agent_cli"] = subprocess.run(
            ["claude", "--version"], capture_output=True, text=True, timeout=30, check=False
        ).stdout.strip()
    except (OSError, subprocess.TimeoutExpired):
        # The version probe is ancillary; it must never discard the paid
        # run's report.
        report["agent_cli"] = "unknown"
    pathlib.Path(arguments.output).write_text(json.dumps(report, indent=1, sort_keys=True) + "\n")
    print(
        f"passed {report['tasks_passed']} of {report['tasks_total']} tasks "
        f"({report['tasks_skipped']} skipped); report at {arguments.output}"
    )
    return 0 if report["tasks_passed"] == report["tasks_total"] else 1


def aggregate_report(surface: str, results: list[dict]) -> dict:
    """Fold per-task results into the recorded report.

    Pure over its input so the aggregation contract is unit-testable. Skipped
    tasks are counted apart from failures and never inflate the pass count.
    """
    usage_total: dict[str, int] = {}
    for result in results:
        for key, value in (result.get("usage") or {}).items():
            if isinstance(value, int):
                usage_total[key] = usage_total.get(key, 0) + value
    return {
        "surface": surface,
        "recorded_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "tasks_total": len(results),
        "tasks_passed": sum(1 for result in results if result["outcome"] == "passed"),
        "tasks_skipped": sum(1 for result in results if result["outcome"] == "skipped"),
        "tool_calls_total": sum(result.get("tool_calls") or 0 for result in results),
        "tool_errors_total": sum(result.get("tool_errors") or 0 for result in results),
        "usage_total": usage_total,
        "cost_usd_total": round(
            sum(result.get("cost_usd") or 0.0 for result in results), 6
        ),
        # Interrupted runs spend tokens whose cost the CLI never reported;
        # the total above cannot include what was never priced, so the report
        # says how many tasks it is missing instead of pretending zero.
        "cost_unaccounted_tasks": sum(
            1
            for result in results
            if result.get("cost_usd") is None and result.get("usage")
        ),
        "results": results,
    }


if __name__ == "__main__":
    sys.exit(main())
