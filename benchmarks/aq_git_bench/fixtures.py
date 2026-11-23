import json
import random
import shutil
import subprocess

from .protocol import ENV, GIT, git, oid, request


def marker_matches(marker, expected):
    return marker.exists() and json.loads(marker.read_text()) == expected


def replace_owned_directory(path, owned, kind):
    if path.exists():
        if not owned():
            raise RuntimeError(f"Refusing to replace an unowned {kind} fixture")
        shutil.rmtree(path)


def fixture(path, commits, refs, seed):
    marker = path / "fixture.json"
    expected = {"version": 1, "commits": commits, "refs": refs, "seed": seed}
    if marker_matches(marker, expected):
        restore_fixture_heads(path, commits)
        return
    replace_owned_directory(path, marker.exists, "repository")
    path.mkdir(parents=True)
    git(path, ["init", "--bare"])
    p = subprocess.Popen(
        [GIT, "fast-import", "--quiet", "--export-marks=" + str(path / "marks")],
        cwd=path,
        env=ENV,
        stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    write_fixture_commits(p.stdin, commits, seed)
    out = p.stdin
    for i in range(max(0, refs - 2)):
        out.write(
            f"reset refs/heads/fixture-{i:05d}\nfrom :{1 + i * 7919 % commits}\n\n".encode()
        )
    out.write(b"done\n")
    out.close()
    stderr = p.stderr.read()
    p.wait()
    if p.returncode:
        raise RuntimeError(stderr.decode())
    git(path, ["repack", "-adq"])
    marker.write_text(json.dumps(expected, sort_keys=True))


def restore_fixture_heads(path, commits):
    marks = {
        int(line.split()[0][1:]): line.split()[1]
        for line in (path / "marks").read_text().splitlines()
    }
    git(
        path,
        [
            "update-ref",
            "refs/heads/main",
            marks[commits if commits % 2 == 0 else commits - 1],
        ],
    )
    git(
        path,
        [
            "update-ref",
            "refs/heads/left",
            marks[commits if commits % 2 else commits - 1],
        ],
    )


def write_fixture_commits(out, commits, seed):
    heads = {}
    for i in range(1, commits + 1):
        branch = "left" if i % 2 else "main"
        other = "main" if branch == "left" else "left"
        message = f"seed {seed} commit {i}\n".encode()
        blob = f"{seed}:{i}\n".encode()
        out.write(
            f"commit refs/heads/{branch}\nmark :{i}\nauthor Fixture <fixture@example.test> {1700000000 + i} +0000\ncommitter Fixture <fixture@example.test> {1700000000 + i} +0000\ndata {len(message)}\n".encode()
            + message
        )
        if branch in heads:
            out.write(f"from :{heads[branch]}\n".encode())
        if i % 20 == 0 and other in heads:
            out.write(f"merge :{heads[other]}\n".encode())
        out.write(
            f"M 100644 inline file-{i % 10:02d}\ndata {len(blob)}\n".encode()
            + blob
            + b"\n"
        )
        heads[branch] = i


def workspace_fixture(path, files):
    marker = path.parent / "workspace-fixture.json"
    expected = {"version": 1, "files": files}
    if marker_matches(marker, expected):
        return

    def owned():
        return marker.exists() or all(is_fixture_file(p) for p in path.iterdir())

    replace_owned_directory(path, owned, "workspace")
    path.mkdir()
    git(path, ["init", "-b", "main"])
    for i in range(files):
        (path / f"file-{i:05d}").write_text(f"fixture {i}\n")
    git(path, ["add", "."])
    git(
        path,
        [
            "-c",
            "user.name=Benchmark",
            "-c",
            "user.email=bench@example.test",
            "commit",
            "-qm",
            "deterministic workspace fixture",
        ],
        env_extra={
            "GIT_AUTHOR_DATE": "1700000000 +0000",
            "GIT_COMMITTER_DATE": "1700000000 +0000",
        },
    )
    marker.write_text(json.dumps(expected))


def make_queries(paths, key_count, seed, limit):
    rng = random.Random(seed)
    histories = {
        repo: git(path, ["rev-list", "--all"]).decode().splitlines()
        for repo, path in paths.items()
    }
    queries = []
    repos = list(paths)
    counts = {
        "inspect": key_count * 40 // 100,
        "contains": key_count * 40 // 100,
        "tree": key_count * 10 // 100,
    }
    counts["diff"] = key_count - sum(counts.values())
    for op, count in counts.items():
        queries.extend(operation_queries(op, count, (rng, repos, histories), limit))
    rng.shuffle(queries)
    return queries


def is_fixture_file(path):
    suffix = path.name.removeprefix("file-")
    if not path.name.startswith("file-"):
        return False
    if not path.is_file() or not suffix.isdigit():
        return False
    return path.read_text() == f"fixture {int(suffix)}\n"


def operation_queries(op, count, sampling, limit):
    rng, repos, histories = sampling
    seen = set()
    while len(seen) < count:
        repo = rng.choice(repos)
        history = histories[repo]
        a, b = rng.sample(history, 2)
        params = {
            "inspect": {"oid": oid(a)},
            "contains": {"ancestor": oid(a), "descendant": oid(b)},
            "tree": {"oid": oid(a), "limit": limit},
            "diff": {"base": oid(a), "target": oid(b), "limit": limit},
        }[op]
        key = json.dumps([repo, op, params], sort_keys=True)
        if key in seen:
            continue
        seen.add(key)
        yield request(op, params, repo)


def disjoint_queries(paths, count, seed, seen):
    targets = {
        "inspect": count * 40 // 100,
        "contains": count * 40 // 100,
        "tree": count * 10 // 100,
        "diff": count * 10 // 100,
    }
    selected = []
    for attempt in range(100):
        for q in make_queries(paths, count, seed + attempt, 100):
            key = json.dumps(
                [q["repository_id"], q["operation"], q["params"]], sort_keys=True
            )
            if targets[q["operation"]] > 0 and key not in seen:
                seen.add(key)
                selected.append(q)
                targets[q["operation"]] -= 1
        if not any(targets.values()):
            return selected
    raise RuntimeError(
        "Fixture cannot supply the requested number of disjoint cold keys"
    )
