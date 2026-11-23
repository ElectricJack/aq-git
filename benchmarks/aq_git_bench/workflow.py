import pathlib
import time
import uuid

from .protocol import git, oid, registration, request
from .service import release


def register_authority(control, work, workspace):
    authority = work / f"workflow-authority-{uuid.uuid4()}.git"
    git(work, ["clone", "--bare", str(workspace), str(authority)])
    repo = str(uuid.uuid4())
    control.execute(
        registration(
            {
                "identity": "benchmark-workflow",
                "mode": "central",
                "remote_url": str(authority),
                "allow_file_transport": True,
                "allowed_refs": ["refs/heads/main"],
                "allowed_targets": ["refs/heads/main"],
            },
            repo,
        )
    )
    head = control.execute(request("repo.sync", {}, repo, True))["refs"][
        "refs/heads/main"
    ]
    return repo, head


def workflow_budgets(control, workspace, work, file_count):
    repo, head = register_authority(control, work, workspace)
    rows = []

    def run(operation, params):
        before = time.perf_counter()
        value = control.execute(request(operation, params, repo, True))
        rows.append(
            {
                "operation": operation,
                "milliseconds": (time.perf_counter() - before) * 1000,
                "result": value,
            }
        )
        return value

    created = run("workspace.create", {"base": oid(head)})
    workspace_id = created["workspace_id"]
    checkpoint = checkpoint_workspace(run, created)
    candidate = create_candidate(run, checkpoint, head)
    published = run(
        "publish",
        {
            "candidate_id": candidate["candidate_id"],
            "target": "refs/heads/main",
            "expected_old": oid(head),
        },
    )
    run(
        "workspace.retire",
        {
            "workspace_id": workspace_id,
            "expected_generation": checkpoint["workspace_generation"],
            "expected_head": checkpoint["source"],
            "quiescent": True,
        },
    )
    release(control, repo, candidate["retention_handle"])
    release(control, repo, checkpoint["retention_handle"])
    return {
        "files": file_count,
        "authority": "temporary local bare Git repository",
        "operations": rows,
        "published_outcome": published,
        "error": None,
    }


def advance_workspace(created):
    path = pathlib.Path(created["path"])
    git(
        path,
        [
            "-c",
            "user.name=Benchmark",
            "-c",
            "user.email=bench@example.test",
            "commit",
            "--allow-empty",
            "-qm",
            "publication budget",
        ],
    )
    new = git(path, ["rev-parse", "HEAD"]).decode().strip()
    return new


def checkpoint_workspace(run, created):
    new = advance_workspace(created)
    return run(
        "workspace.checkpoint",
        {
            "workspace_id": created["workspace_id"],
            "expected_generation": created["generation"],
            "expected_head": oid(new),
            "quiescent": True,
            "mode": "head",
        },
    )


def create_candidate(run, checkpoint, head):
    return run(
        "candidate.create",
        {
            "base": oid(head),
            "sources": [
                {
                    "oid": checkpoint["source"],
                    "retention_handle": checkpoint["retention_handle"],
                }
            ],
            "commit": {
                "name": "Benchmark",
                "email": "bench@example.test",
                "message": "workflow candidate",
            },
        },
    )
