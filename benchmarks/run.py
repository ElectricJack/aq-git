#!/usr/bin/env python3
"""Reproducible standalone Git/socket benchmark. No third party Python packages."""

import argparse
import csv
import hashlib
import json
import os
import pathlib
import platform
import random
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import threading
import time
import uuid
import multiprocessing
import queue as thread_queue
import shlex
import tarfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
SERVERS = []
ENV = {
    "PATH": "/usr/bin:/bin",
    "HOME": "/nonexistent",
    "LANG": "C",
    "LC_ALL": "C",
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_CONFIG_GLOBAL": "/dev/null",
    "GIT_NO_REPLACE_OBJECTS": "1",
    "GIT_TERMINAL_PROMPT": "0",
}
GIT = shutil.which("git", path=ENV["PATH"])


def git(path, args, data=None, env_extra=None):
    p = subprocess.run(
        [GIT, "-c", "core.hooksPath=/dev/null", "-c", "gc.auto=0", *args],
        cwd=path,
        input=data,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=dict(ENV, **env_extra or {}),
    )
    if p.returncode:
        raise RuntimeError(
            f"Git {args[0]} failed ({p.returncode}): {p.stderr.decode(errors='replace')[:500]}"
        )
    return p.stdout


def oid(h):
    return {"algorithm": "sha1", "hex": h}


def byte_path(b):
    try:
        return {"encoding": "utf8", "value": b.decode()}
    except UnicodeDecodeError:
        return {"encoding": "hex", "value": b.hex()}


def request(op, params, repo=None, mutation=False, generation=1):
    r = {
        "protocol_version": 1,
        "request_id": str(uuid.uuid4()),
        "operation": op,
        "params": params,
    }
    if repo:
        r.update(repository_id=repo, expected_binding_generation=generation)
    if mutation:
        r["operation_id"] = str(uuid.uuid4())
    return r


class Client:
    def __init__(self, path):
        self.path = path
        self.reconnects = 0
        self._connect()

    def _connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(350)
        self.sock.connect(str(self.path))
        self.reader = self.sock.makefile("rb")
        self.last_activity = time.monotonic()

    def _reconnect(self):
        self.close()
        self._connect()
        self.reconnects += 1

    def execute(self, r):
        if time.monotonic() - self.last_activity >= 25:
            self._reconnect()
        try:
            return self._exchange(r)
        except ConnectionError:
            self._reconnect()
            # Retrying uses the identical request and mutation operation ID.
            return self._exchange(r)

    def _exchange(self, r):
        self.sock.sendall(json.dumps(r, separators=(",", ":")).encode() + b"\n")
        line = self.reader.readline(8 * 1024 * 1024 + 1)
        if not line:
            raise ConnectionError("socket closed")
        self.last_activity = time.monotonic()
        response = json.loads(line)
        if not response["ok"]:
            raise RuntimeError(json.dumps(response["error"]))
        if response.get("request_id") != r["request_id"]:
            raise RuntimeError("response request ID mismatch")
        return response["result"]

    def close(self):
        self.reader.close()
        self.sock.close()


class GitBaseline:
    def __init__(self, paths, persistent=False):
        self.paths = paths
        self.persistent = persistent
        self.readers = {}

    def object(self, path, h):
        if not self.persistent:
            return git(path, ["cat-file", "commit", h])
        if path not in self.readers:
            self.readers[path] = subprocess.Popen(
                [GIT, "cat-file", "--batch-command"],
                cwd=path,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                env=ENV,
            )
        p = self.readers[path]
        p.stdin.write(f"contents {h}\n".encode())
        p.stdin.flush()
        header = p.stdout.readline().split()
        if len(header) != 3 or header[0].decode() != h or header[1] != b"commit":
            raise RuntimeError("invalid baseline batch frame")
        data = p.stdout.read(int(header[2]))
        assert p.stdout.read(1) == b"\n"
        return data

    def execute(self, q):
        path = self.paths[q["repository_id"]]
        p = q["params"]
        op = q["operation"]
        if op == "inspect":
            h = p["oid"]["hex"]
            raw = self.object(path, h)
            header, _, message = raw.partition(b"\n\n")
            result = {
                "oid": oid(h),
                "tree": None,
                "parents": [],
                "author": None,
                "committer": None,
                "message": byte_path(message),
            }
            for line in header.splitlines():
                if line.startswith(b"tree "):
                    result["tree"] = oid(line[5:].decode())
                elif line.startswith(b"parent "):
                    result["parents"].append(oid(line[7:].decode()))
                elif line.startswith(b"author "):
                    result["author"] = byte_path(line[7:])
                elif line.startswith(b"committer "):
                    result["committer"] = byte_path(line[10:])
            return result
        if op == "contains":
            answer = subprocess.run(
                [
                    GIT,
                    "merge-base",
                    "--is-ancestor",
                    p["ancestor"]["hex"],
                    p["descendant"]["hex"],
                ],
                cwd=path,
                env=ENV,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
            )
            if answer.returncode not in (0, 1):
                raise RuntimeError("baseline ancestry error")
            return {
                "contains": answer.returncode == 0,
                "semantics": "strict_commit_ancestry_v1",
            }
        if op == "tree":
            args = (
                ["ls-tree", "-z"]
                + (["-r"] if p.get("recursive") else [])
                + [p["oid"]["hex"]]
            )
            data = git(path, args)
            entries = []
            for item in data.split(b"\x00"):
                if not item:
                    continue
                header, name = item.split(b"\t", 1)
                mode, kind, h = header.decode().split(" ")
                entries.append(
                    {"mode": mode, "type": kind, "oid": oid(h), "path": byte_path(name)}
                )
        elif op == "diff":
            data = git(
                path,
                [
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-renames",
                    "--name-status",
                    "-z",
                    p["base"]["hex"],
                    p["target"]["hex"],
                    "--",
                ],
            )
            items = data.split(b"\x00")
            entries = [
                {"status": items[i].decode(), "path": byte_path(items[i + 1])}
                for i in range(0, len(items) - 1, 2)
            ]
        else:
            raise RuntimeError("unrecognized baseline query")
        limit = p.get("limit", 100)
        return {"entries": entries[:limit], "truncated": len(entries) > limit}

    def close(self):
        for p in self.readers.values():
            p.stdin.close()
            try:
                p.wait(timeout=2)
            except subprocess.TimeoutExpired:
                p.kill()
                p.wait()


def fixture(path, commits, refs, seed):
    marker = path / "fixture.json"
    expected = {"version": 1, "commits": commits, "refs": refs, "seed": seed}
    if marker.exists() and json.loads(marker.read_text()) == expected:
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
        return
    if path.exists():
        if not marker.exists():
            raise RuntimeError("Refusing to replace an unowned repository fixture")
        shutil.rmtree(path)
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
    heads = {}
    out = p.stdin
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


def workspace_fixture(path, files):
    marker = path.parent / "workspace-fixture.json"
    expected = {"version": 1, "files": files}
    if marker.exists() and json.loads(marker.read_text()) == expected:
        return
    if path.exists():
        owned = marker.exists() or all(
            (
                p.is_file()
                and p.name.startswith("file-")
                and p.name[5:].isdigit()
                and (p.read_text() == f"fixture {int(p.name[5:])}\n")
                for p in path.iterdir()
            )
        )
        if not owned:
            raise RuntimeError("Refusing to replace an unowned workspace fixture")
        shutil.rmtree(path)
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
            queries.append(request(op, params, repo))
    rng.shuffle(queries)
    return queries


def percentile(values, p):
    if not values:
        return None
    return sorted(values)[min(len(values) - 1, int((len(values) - 1) * p))]


def summarize(samples, elapsed, errors):
    lat = [x[1] for x in samples]
    return {
        "successful_queries": len(lat),
        "errors": errors,
        "seconds": elapsed,
        "queries_per_second": len(lat) / elapsed,
        "p50_ms": percentile(lat, 0.5),
        "p95_ms": percentile(lat, 0.95),
        "p99_ms": percentile(lat, 0.99),
        "classes": {
            op: {
                "count": len(v),
                "queries_per_second": len(v) / elapsed,
                "p50_ms": percentile(v, 0.5),
                "p95_ms": percentile(v, 0.95),
                "p99_ms": percentile(v, 0.99),
            }
            for op in ("inspect", "contains", "tree", "diff", "batch")
            if (v := [x[1] for x in samples if x[0] == op])
        },
    }


def measure(
    kind,
    queries,
    paths,
    sock,
    clients,
    seconds,
    seed,
    raw_path,
    arrival_rate=0,
    batch=False,
    unique=False,
    background=None,
    service_pid=None,
    unique_budget_seconds=300,
):
    context = multiprocessing.get_context("fork")
    barrier = context.Barrier(clients + 1)
    start_value = context.Value("d", 0.0)
    queue = context.Queue()

    def worker(index):
        answer_digests = {}
        if unique:
            os.setsid()
        rng = random.Random(seed + index)
        samples = []
        errors = []
        error_count = 0
        n = 0
        owned = queries[index::clients]
        try:
            executor = (
                Client(sock)
                if kind == "service"
                else GitBaseline(paths, kind == "git_persistent")
            )
        except Exception as e:
            barrier.wait()
            queue.put(([], [str(e)], 1))
            return
        barrier.wait()
        start = start_value.value
        end = start + (unique_budget_seconds if unique else seconds)
        try:
            with open(str(raw_path) + f".{index}.csv", "w", newline="") as stream:
                raw = csv.writer(stream)
                raw.writerow(["operation", "latency_ms", "success", "error"])
                while n < len(owned) if unique else time.perf_counter() < end:
                    scheduled = (
                        start + n * clients / arrival_rate
                        if arrival_rate
                        else time.perf_counter()
                    )
                    if time.perf_counter() >= end or (not unique and scheduled > end):
                        if unique and n < len(owned):
                            error_count += 1
                            errors.append(
                                "finite cold workload budget exceeded before all keys completed"
                            )
                        break
                    delay = scheduled - time.perf_counter()
                    if delay > 0:
                        time.sleep(delay)
                    q = owned[n] if unique else rng.choice(queries)
                    op = q["operation"]
                    if batch:
                        repo = q["repository_id"]
                        items = [x for x in queries if x["repository_id"] == repo]
                        chosen = [rng.choice(items) for _ in range(100)]
                        q = request(
                            "query.batch",
                            {
                                "items": [
                                    {"operation": x["operation"], "params": x["params"]}
                                    for x in chosen
                                ]
                            },
                            repo,
                        )
                        op = "batch"
                    before = scheduled if arrival_rate else time.perf_counter()
                    try:
                        if batch and kind != "service":
                            for x in chosen:
                                executor.execute(x)
                        else:
                            answer = executor.execute(q)
                        ms = (time.perf_counter() - before) * 1000
                        if unique:
                            # Hash decoded answers after stopping the latency clock.
                            answer_digests[str(index + n * clients)] = hashlib.sha256(
                                json.dumps(
                                    answer,
                                    sort_keys=True,
                                    separators=(",", ":"),
                                    ensure_ascii=True,
                                ).encode()
                            ).hexdigest()
                        samples.append((op, ms))
                        raw.writerow([op, round(ms, 6), 1, ""])
                    except Exception as e:
                        error_count += 1
                        if len(errors) < 20:
                            errors.append(str(e))
                        raw.writerow(
                            [op, (time.perf_counter() - before) * 1000, 0, str(e)]
                        )
                        time.sleep(0.001)
                    n += 1
        finally:
            executor.close()
        if unique:
            pathlib.Path(str(raw_path) + f".{index}.answers.json").write_text(
                json.dumps(answer_digests, sort_keys=True)
            )
        queue.put((samples, errors, error_count))

    workers = [context.Process(target=worker, args=(i,)) for i in range(clients)]
    for child in workers:
        child.start()
    monitor_stop = threading.Event()
    resource_summary = {"peak_sampled_rss_kib_sum": 0, "samples": 0}
    rss_samples = []

    def monitor():
        with open(str(raw_path) + ".resources.csv", "w", newline="") as stream:
            writer = csv.writer(stream)
            writer.writerow(
                ["monotonic_seconds", "rss_kib_sum", "process_count", "cpu_seconds_sum"]
            )
            while not monitor_stop.is_set():
                trees = [
                    process_tree(pid)
                    for pid in [
                        service_pid,
                        *getattr(background, "additional_pids", []),
                    ]
                ]
                tree = {
                    "current_rss_kib_sum": sum(
                        (t["current_rss_kib_sum"] for t in trees)
                    ),
                    "processes": [p for t in trees for p in t["processes"]],
                }
                rss = tree["current_rss_kib_sum"]
                rss_samples.append(rss)
                resource_summary["peak_sampled_rss_kib_sum"] = max(
                    resource_summary["peak_sampled_rss_kib_sum"], rss
                )
                resource_summary["samples"] += 1
                writer.writerow(
                    [
                        time.perf_counter(),
                        rss,
                        len(tree["processes"]),
                        sum(
                            (
                                p["user_seconds"] + p["system_seconds"]
                                for p in tree["processes"]
                            )
                        ),
                    ]
                )
                monitor_stop.wait(0.1)

    monitor_thread = threading.Thread(target=monitor) if service_pid else None
    if monitor_thread:
        monitor_thread.start()
    cleanup = background() if background else None
    start_value.value = time.perf_counter()
    barrier.wait()
    if unique:
        deadline = time.monotonic() + unique_budget_seconds + 1
        results = []
        try:
            for _ in workers:
                results.append(
                    queue.get(timeout=max(0.001, deadline - time.monotonic()))
                )
        except thread_queue.Empty:
            for child in workers:
                if child.is_alive():
                    try:
                        os.killpg(child.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
            results.append(
                ([], ["finite cold workload exceeded overall bounded deadline"], 1)
            )
    else:
        results = [queue.get(timeout=seconds + 350) for _ in workers]
    for child in workers:
        child.join(timeout=5)
    measured_end = time.perf_counter()
    background_result = cleanup(start_value.value, measured_end) if cleanup else None
    monitor_stop.set()
    if monitor_thread:
        monitor_thread.join()
    if rss_samples:
        width = max(1, len(rss_samples) // 10)
        resource_summary.update(
            first_rss_kib_sum=rss_samples[0],
            last_rss_kib_sum=rss_samples[-1],
            steady_rss_drift_kib=statistics.mean(rss_samples[-width:])
            - statistics.mean(rss_samples[-2 * width : -width] or rss_samples[:width]),
        )
    elapsed = measured_end - start_value.value
    summary = summarize(
        [v for samples, _, _ in results for v in samples],
        elapsed,
        [e for _, errors, _ in results for e in errors][:20],
    )
    summary["error_count"] = sum((count for _, _, count in results))
    if unique:
        digests = {}
        for index in range(clients):
            answer_path = pathlib.Path(str(raw_path) + f".{index}.answers.json")
            if answer_path.exists():
                digests.update(json.loads(answer_path.read_text()))
        summary["answer_digests"] = digests
        summary["finite_workload"] = {
            "expected_keys": len(queries),
            "completed_keys": summary["successful_queries"],
            "budget_seconds": unique_budget_seconds,
            "complete": summary["successful_queries"] == len(queries)
            and summary["error_count"] == 0,
        }
    summary["requests_per_second"] = summary["queries_per_second"]
    summary["logical_queries_per_second"] = summary["queries_per_second"] * (
        100 if batch else 1
    )
    summary["logical_queries"] = summary["successful_queries"] * (100 if batch else 1)
    summary["driver"] = "one Python OS process per client"
    summary["background"] = background_result
    summary["service_resources"] = resource_summary if service_pid else None
    return summary


def process_tree(pid):
    records = []
    pending = [pid]
    while pending:
        current = pending.pop()
        try:
            status = pathlib.Path(f"/proc/{current}/status").read_text()
            stat = pathlib.Path(f"/proc/{current}/stat").read_text()
            fields = stat[stat.rfind(")") + 2 :].split()
            memory = {
                line.split(":", 1)[0]: line.split(":", 1)[1].strip()
                for line in status.splitlines()
                if line.startswith(("Name:", "VmRSS:", "VmHWM:"))
            }
            records.append(
                {
                    "pid": current,
                    **memory,
                    "user_seconds": int(fields[11]) / os.sysconf("SC_CLK_TCK"),
                    "system_seconds": int(fields[12]) / os.sysconf("SC_CLK_TCK"),
                }
            )
            pending.extend(
                (
                    int(v)
                    for v in pathlib.Path(f"/proc/{current}/task/{current}/children")
                    .read_text()
                    .split()
                )
            )
        except (FileNotFoundError, ProcessLookupError):
            pass
    return {
        "processes": records,
        "current_rss_kib_sum": sum(
            (int(p.get("VmRSS", "0 kB").split()[0]) for p in records)
        ),
        "note": "Per-process VmHWM is reported; sum of individual high-water marks is not simultaneous peak RSS.",
    }


def disjoint_queries(paths, count, seed, limit, seen):
    targets = {
        "inspect": count * 40 // 100,
        "contains": count * 40 // 100,
        "tree": count * 10 // 100,
        "diff": count * 10 // 100,
    }
    selected = []
    for attempt in range(100):
        for q in make_queries(paths, count, seed + attempt, limit):
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


def retain(control, repo, head):
    return control.execute(
        request(
            "candidate.create",
            {
                "base": oid(head),
                "sources": [],
                "commit": {
                    "name": "Benchmark",
                    "email": "bench@example.test",
                    "message": "retained benchmark input",
                },
            },
            repo,
            True,
        )
    )["retention_handle"]


def release(control, repo, handle):
    record = control.execute(request("retention.inspect", {"handle_id": handle}, repo))
    return control.execute(
        request(
            "retention.release",
            {
                "handle_id": handle,
                "expected_generation": record["generation"],
                "refs": record["refs"],
                "reason": "benchmark finished using resource",
            },
            repo,
            True,
        )
    )


def start_server(binary, state, sock, log, git_executable=None):
    command = [str(binary.resolve()), "--state-dir", str(state), "--listen", str(sock)]
    if git_executable:
        command += ["--git-executable", str(git_executable)]
    process = subprocess.Popen(command + ["serve"], stdout=log, stderr=log)
    SERVERS.append(process)
    for _ in range(1000):
        if process.poll() is not None:
            raise RuntimeError("service exited before opening socket")
        if sock.exists():
            return process
        time.sleep(0.01)
    raise RuntimeError("service socket startup deadline")


def restart_server(process, control, binary, state, sock, log, git_executable=None):
    control.close()
    process.send_signal(signal.SIGTERM)
    try:
        process.wait(timeout=15)
    except subprocess.TimeoutExpired:
        raise RuntimeError("Service did not shut down cleanly for benchmark restart")
    sock.unlink(missing_ok=True)
    process = start_server(binary, state, sock, log, git_executable)
    control = Client(sock)
    control.execute(
        request(
            "repo.register",
            {"identity": "benchmark-private-probe", "mode": "worker"},
            str(uuid.uuid4()),
            True,
            0,
        )
    )
    return (process, control)


def candidate_fixture(base_path, head, work, count=64):
    directory = work / f"candidate-sources-{uuid.uuid4()}.git"
    git(work, ["clone", "--bare", str(base_path), str(directory)])
    commands = bytearray()
    refs = []
    for i in range(count):
        ref = f"refs/heads/mixed-source-{i:02d}"
        refs.append(ref)
        message = f"independent mixed source {i}\n".encode()
        blob = f"source {i}\n".encode()
        commands.extend(
            f"commit {ref}\nauthor Benchmark <bench@example.test> {1701000000 + i} +0000\ncommitter Benchmark <bench@example.test> {1701000000 + i} +0000\ndata {len(message)}\n".encode()
            + message
            + f"from {head}\nM 100644 inline mixed-source-{i:02d}.txt\ndata {len(blob)}\n".encode()
            + blob
            + b"\n"
        )
    commands.extend(b"done\n")
    git(directory, ["fast-import", "--quiet"], bytes(commands))
    return directory, refs


def mixed_setup(
    control,
    sock,
    paths,
    state,
    work,
    binary,
    log,
    stall_remote,
    stall_release,
    stall_marker,
):
    repos = list(paths)
    repo_a, repo_b = repos[:2]
    head_a = control.execute(request("repo.sync", {}, repo_a, True))["refs"][
        "refs/heads/main"
    ]
    head_b = control.execute(request("repo.sync", {}, repo_b, True))["refs"][
        "refs/heads/main"
    ]
    handle_a = retain(control, repo_a, head_a)
    candidate_remote, candidate_refs = candidate_fixture(paths[repo_b], head_b, work)
    candidate_repo = str(uuid.uuid4())
    control.execute(
        request(
            "repo.register",
            {
                "identity": "benchmark-candidate-" + candidate_repo,
                "mode": "central",
                "remote_url": str(candidate_remote),
                "allow_file_transport": True,
                "allowed_refs": ["refs/heads/main", *candidate_refs],
            },
            candidate_repo,
            True,
            0,
        )
    )
    candidate_observation = control.execute(
        request("repo.sync", {}, candidate_repo, True)
    )
    candidate_sources = [
        {
            "oid": oid(candidate_observation["refs"][ref]),
            "retention_handle": retain(
                control, candidate_repo, candidate_observation["refs"][ref]
            ),
        }
        for ref in candidate_refs
    ]
    worker_state = work / f"mixed-worker-{uuid.uuid4()}"
    worker_socket = work / f"mixed-{os.getpid()}.sock"
    worker = start_server(binary, worker_state, worker_socket, log)
    wc = Client(worker_socket)
    wc.execute(
        request(
            "repo.register",
            {"identity": "benchmark-" + repo_a, "mode": "worker"},
            repo_a,
            True,
            0,
        )
    )
    wc.close()
    stall_repo = str(uuid.uuid4())
    control.execute(
        request(
            "repo.register",
            {
                "identity": "benchmark-stall",
                "mode": "central",
                "remote_url": str(stall_remote),
                "allow_file_transport": True,
                "allowed_refs": ["refs/heads/main"],
            },
            stall_repo,
            True,
            0,
        )
    )

    def background():
        stop = threading.Event()
        handoffs = thread_queue.Queue(maxsize=2)
        events = []
        events_lock = threading.Lock()
        began = time.perf_counter()

        def record(kind, start, error=None, completed=None):
            with events_lock:
                events.append(
                    {
                        "kind": kind,
                        "started_seconds": start - began,
                        "completed_seconds": (
                            completed if completed is not None else time.perf_counter()
                        )
                        - began,
                        "error": error,
                    }
                )

        def exporter():
            client = Client(sock)
            try:
                while not stop.is_set():
                    start = time.perf_counter()
                    try:
                        out = client.execute(
                            request(
                                "artifact.export",
                                {
                                    "source": oid(head_a),
                                    "base": oid(head_a),
                                    "retention_handle": handle_a,
                                },
                                repo_a,
                                True,
                            )
                        )
                        completed = time.perf_counter()
                        handle = out["artifact_handle"]
                        incoming = worker_state / "artifacts/incoming" / handle
                        incoming.parent.mkdir(parents=True, exist_ok=True)
                        shutil.copytree(state / "artifacts" / handle, incoming)
                        while not stop.is_set():
                            try:
                                handoffs.put(handle, timeout=0.05)
                                break
                            except thread_queue.Full:
                                pass
                        record("export", start, completed=completed)
                    except Exception as e:
                        record("export", start, str(e))
                        break
            finally:
                client.close()

        def importer():
            client = Client(worker_socket)
            try:
                while not stop.is_set() or not handoffs.empty():
                    try:
                        handle = handoffs.get(timeout=0.05)
                    except thread_queue.Empty:
                        continue
                    start = time.perf_counter()
                    try:
                        client.execute(
                            request(
                                "artifact.import",
                                {"artifact_handle": handle},
                                repo_a,
                                True,
                            )
                        )
                        completed = time.perf_counter()
                        release(client, repo_a, handle)
                        central = Client(sock)
                        release(central, repo_a, handle)
                        central.close()
                        record("import", start, completed=completed)
                    except Exception as e:
                        record("import", start, str(e))
                        break
            finally:
                client.close()

        def candidate():
            client = Client(sock)
            try:
                while not stop.is_set():
                    start = time.perf_counter()
                    try:
                        out = client.execute(
                            request(
                                "candidate.create",
                                {
                                    "base": oid(head_b),
                                    "sources": candidate_sources,
                                    "commit": {
                                        "name": "Benchmark",
                                        "email": "bench@example.test",
                                        "message": "mixed candidate",
                                    },
                                },
                                candidate_repo,
                                True,
                            )
                        )
                        completed = time.perf_counter()
                        release(client, candidate_repo, out["retention_handle"])
                        record("candidate", start, completed=completed)
                    except Exception as e:
                        record("candidate", start, str(e))
                        break
            finally:
                client.close()

        def stalled():
            client = Client(sock)
            start = time.perf_counter()
            try:
                client.execute(request("repo.sync", {}, stall_repo, True))
                record("stalled_fetch", start)
            except Exception as e:
                record("stalled_fetch", start, str(e))
            finally:
                client.close()

        threads = [
            threading.Thread(target=f) for f in (exporter, importer, candidate, stalled)
        ]
        for thread in threads:
            thread.start()
        deadline = time.monotonic() + 10
        while not stall_marker.exists() and time.monotonic() < deadline:
            time.sleep(0.01)

        def cleanup(measured_start, measured_end):
            stop.set()
            stall_release.touch()
            for thread in threads:
                thread.join(timeout=330)
            cleanup_errors = []
            cleanup_client = Client(sock)
            for resource_repo, handle in [
                (repo_a, handle_a),
                *[
                    (candidate_repo, source["retention_handle"])
                    for source in candidate_sources
                ],
            ]:
                try:
                    release(cleanup_client, resource_repo, handle)
                except Exception as error:
                    cleanup_errors.append(str(error))
            cleanup_client.close()
            worker_tree = process_tree(worker.pid)
            worker.send_signal(signal.SIGTERM)
            try:
                worker.wait(timeout=10)
            except subprocess.TimeoutExpired:
                worker.kill()
                worker.wait()
            lo = measured_start - began
            hi = measured_end - began
            common = [(lo, hi)]
            overlaps = {}
            for kind in ("export", "import", "candidate", "stalled_fetch"):
                intervals = [
                    (
                        max(lo, event["started_seconds"]),
                        min(hi, event["completed_seconds"]),
                    )
                    for event in events
                    if event["kind"] == kind
                    and not event["error"]
                    and event["started_seconds"] < hi
                    and event["completed_seconds"] > lo
                ]
                overlaps[kind] = sum(end - start for start, end in intervals)
                common = [
                    (max(start, other_start), min(end, other_end))
                    for start, end in common
                    for other_start, other_end in intervals
                    if max(start, other_start) < min(end, other_end)
                ]
            common_seconds = sum(end - start for start, end in common)
            return {
                "operations": events,
                "measurement_window_seconds": {"start": lo, "end": hi},
                "measured_overlap_seconds_by_kind": overlaps,
                "four_way_overlap_seconds": common_seconds,
                "four_way_overlap_fraction": common_seconds / (hi - lo),
                "candidate_workload": {
                    "sources": len(candidate_sources),
                    "unique_sources": len(
                        {source["oid"]["hex"] for source in candidate_sources}
                    ),
                    "shape": "independent commits from one base, each adding a distinct file; 63 real clean merge steps",
                },
                "cleanup_errors": cleanup_errors,
                "stalled_fetch_entered": stall_marker.exists(),
                "worker_process_tree": worker_tree,
                "all_four_classes_progressed": all(
                    (
                        any((e["kind"] == kind and (not e["error"]) for e in events))
                        for kind in ("export", "import", "candidate", "stalled_fetch")
                    )
                ),
                "note": "Export/import pipeline is bounded to two queued bundles; retained-resource and filesystem usage is reported, not silently deleted.",
            }

        return cleanup

    background.additional_pids = [worker.pid]
    return background


def agent_loop(control, sock, paths, clients, seconds, backend, output):
    context = multiprocessing.get_context("fork")
    inboxes = [context.Queue() for _ in range(clients)]
    outbox = context.Queue()

    def worker(index):
        executor = (
            Client(sock)
            if backend == "service"
            else GitBaseline(paths, backend == "git_persistent")
        )
        try:
            while True:
                payload = inboxes[index].get()
                if payload is None:
                    break
                round_id, queries = payload
                start = time.perf_counter()
                try:
                    if backend == "service":
                        answer = executor.execute(
                            request(
                                "query.batch",
                                {
                                    "items": [
                                        {
                                            "operation": q["operation"],
                                            "params": q["params"],
                                        }
                                        for q in queries
                                    ]
                                },
                                queries[0]["repository_id"],
                            )
                        )
                        values = [item["result"] for item in answer["items"]]
                    else:
                        values = [executor.execute(q) for q in queries]
                    outbox.put(
                        (
                            round_id,
                            index,
                            (time.perf_counter() - start) * 1000,
                            values,
                            None,
                        )
                    )
                except Exception as e:
                    outbox.put(
                        (
                            round_id,
                            index,
                            (time.perf_counter() - start) * 1000,
                            None,
                            str(e),
                        )
                    )
        finally:
            executor.close()

    workers = [context.Process(target=worker, args=(i,)) for i in range(clients)]
    for child in workers:
        child.start()
    start = time.perf_counter()
    rounds = []
    repo = next(iter(paths))
    path = paths[repo]
    previous = git(path, ["rev-parse", "refs/heads/main"]).decode().strip()
    original_head = previous
    checker = GitBaseline(paths, True)
    try:
        while time.perf_counter() - start < seconds:
            sync_start = time.perf_counter()
            observation = control.execute(request("repo.sync", {}, repo, True))
            sync_ms = (time.perf_counter() - sync_start) * 1000
            frozen = observation["refs"]["refs/heads/main"]
            tree = git(path, ["rev-parse", frozen + "^{tree}"]).decode().strip()
            new = (
                git(
                    path,
                    [
                        "-c",
                        "user.name=Benchmark",
                        "-c",
                        "user.email=bench@example.test",
                        "commit-tree",
                        tree,
                        "-p",
                        frozen,
                    ],
                    b"controlled advance after observation\n",
                )
                .decode()
                .strip()
            )
            git(path, ["update-ref", "refs/heads/main", new, frozen])
            mix = (
                [request("inspect", {"oid": oid(frozen)}, repo)] * 40
                + [
                    request(
                        "contains",
                        {"ancestor": oid(previous), "descendant": oid(frozen)},
                        repo,
                    )
                ]
                * 40
                + [request("tree", {"oid": oid(frozen), "limit": 100}, repo)] * 10
                + [
                    request(
                        "diff",
                        {"base": oid(previous), "target": oid(frozen), "limit": 100},
                        repo,
                    )
                ]
                * 10
            )
            expected = [checker.execute(q) for q in mix]
            round_id = len(rounds)
            decision_start = time.perf_counter()
            for inbox in inboxes:
                inbox.put((round_id, mix))
            answers = [outbox.get(timeout=350) for _ in workers]
            decision_ms = (time.perf_counter() - decision_start) * 1000
            errors = [
                a[4] or "frozen result mismatch"
                for a in answers
                if a[4] or a[3] != expected
            ]
            rounds.append(
                {
                    "round": round_id,
                    "observation_id": observation["observation_id"],
                    "frozen": frozen,
                    "advanced_after_observation": new,
                    "sync_ms": sync_ms,
                    "decision_ms": decision_ms,
                    "client_ms": [a[2] for a in answers],
                    "errors": errors,
                }
            )
            previous = frozen
    finally:
        checker.close()
        git(path, ["update-ref", "refs/heads/main", original_head])
        for inbox in inboxes:
            inbox.put(None)
        for child in workers:
            child.join(timeout=5)
    (output / f"agent-loop-{clients}-{backend}.json").write_text(
        json.dumps(rounds, indent=2)
    )
    return {
        "scenario": "agent-loop",
        "backend": backend,
        "clients": clients,
        "rounds": len(rounds),
        "queries_per_round": 100 * clients,
        "errors": [e for r in rounds for e in r["errors"]],
        "decision_p95_ms": percentile([r["decision_ms"] for r in rounds], 0.95),
        "sync_p95_ms": percentile([r["sync_ms"] for r in rounds], 0.95),
        "note": "All backends share the same explicit service sync contract; local read comparisons are separate from refresh timing. Remote advances occur strictly after each frozen observation.",
    }


def workflow_budgets(control, workspace, work, file_count):
    authority = work / f"workflow-authority-{uuid.uuid4()}.git"
    git(work, ["clone", "--bare", str(workspace), str(authority)])
    repo = str(uuid.uuid4())
    control.execute(
        request(
            "repo.register",
            {
                "identity": "benchmark-workflow",
                "mode": "central",
                "remote_url": str(authority),
                "allow_file_transport": True,
                "allowed_refs": ["refs/heads/main"],
                "allowed_targets": ["refs/heads/main"],
            },
            repo,
            True,
            0,
        )
    )
    head = control.execute(request("repo.sync", {}, repo, True))["refs"][
        "refs/heads/main"
    ]
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
    checkpoint = run(
        "workspace.checkpoint",
        {
            "workspace_id": workspace_id,
            "expected_generation": created["generation"],
            "expected_head": oid(new),
            "quiescent": True,
            "mode": "head",
        },
    )
    candidate = run(
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


def burst(sock, queries, output, on_started=None):
    context = multiprocessing.get_context("fork")
    barrier = context.Barrier(len(queries) + 1)
    queue = context.Queue()

    def worker(index):
        client = Client(sock)
        barrier.wait()
        start = time.perf_counter()
        try:
            answer = {"result": client.execute(queries[index]), "error": None}
        except Exception as error:
            answer = {"result": None, "error": str(error)}
        client.close()
        answer.update(index=index, latency_ms=(time.perf_counter() - start) * 1000)
        queue.put(answer)

    workers = [context.Process(target=worker, args=(i,)) for i in range(len(queries))]
    for child in workers:
        child.start()
    barrier.wait()
    if on_started:
        on_started()
    answers = [queue.get(timeout=350) for _ in workers]
    for child in workers:
        child.join(timeout=5)
    output.write_text(json.dumps(sorted(answers, key=lambda x: x["index"]), indent=2))
    return answers


def cold_answer_agreement(service, baseline):
    expected = service.get("finite_workload", {}).get("expected_keys", 0)
    actual = service.get("answer_digests", {})
    reference = baseline.get("answer_digests", {})
    mismatches = sorted(
        k for k in actual.keys() | reference.keys() if actual.get(k) != reference.get(k)
    )
    return {
        "expected_keys": expected,
        "service_answers": len(actual),
        "baseline_answers": len(reference),
        "mismatched_indices": mismatches,
        "pass": expected > 0
        and len(actual) == expected
        and len(reference) == expected
        and not mismatches
        and service.get("finite_workload", {}).get("complete", False)
        and baseline.get("finite_workload", {}).get("complete", False),
        "method": "SHA-256 of canonical decoded JSON per input index, computed after latency clock stops",
    }


def acceptance_matrix(result):
    measurements = result["measurements"]
    gates = result.get("gates", [])
    required = set((1, 8, 32))
    settings = result["overrides"]
    expected = settings["repetitions"] * len(required)
    read = [m for m in measurements if m["scenario"] == "read"]
    batch = [m for m in measurements if m["scenario"] == "batch"]
    service_read = [m for m in read if m["backend"] == "service"]
    service_batch = [m for m in batch if m["backend"] == "service"]
    read_gates = [g for g in gates if g["scenario"] == "read"]
    batch_gates = [g for g in gates if g["scenario"] == "batch"]
    cold_gates = [g for g in gates if g["scenario"] == "cold"]
    mixed_gates = [g for g in gates if g["scenario"] == "mixed"]
    churn = [m for m in measurements if m["scenario"] == "churn"]
    overload = [m for m in measurements if m["scenario"] == "overload"]
    rep = settings["repetitions"]
    return {
        "reference_fixture": result["reference_sized_fixture"],
        "declared_machine": bool(
            result["environment"].get("cpu_model")
            and result["environment"].get("filesystem")
        ),
        "durations_and_repetitions": settings["seconds"] >= 60
        and rep >= 5
        and required.issubset(settings["clients"]),
        "exact_query_agreement": not result["checks"]["correctness"]["mismatches"],
        "p1": len(service_read) >= expected
        and len(service_batch) >= expected
        and all(
            (g["latency_pass"] and g["zero_errors"] for g in read_gates + batch_gates)
        ),
        "p2": len([g for g in read_gates if g["clients"] == 32]) >= rep
        and all((g.get("p2_pass", False) for g in read_gates if g["clients"] == 32)),
        "p3": settings["seconds"] >= 60
        and len([g for g in read_gates if g["clients"] == 32]) >= rep
        and all((g.get("p3_pass", False) for g in read_gates if g["clients"] == 32)),
        "p4_warm": result["checks"]["warm_git_work"]["equal"],
        "p4_cold": result["checks"].get("singleflight", {}).get("pass", False),
        "p4_sync": result["checks"].get("shared_sync", {}).get("pass", False),
        "p5": settings["mixed_seconds"] >= 60
        and rep >= 5
        and len(mixed_gates) >= rep
        and all((g.get("p5_pass", False) for g in mixed_gates)),
        "p6": len([g for g in cold_gates if g["clients"] == 1]) >= rep
        and all((g.get("p6_pass", False) for g in cold_gates if g["clients"] == 1)),
        "p7": len(churn) >= rep
        and all(
            (
                m["seconds"] >= 1800
                and m.get("boundedness", {}).get("evictions_observed", False)
                and m.get("boundedness", {}).get("cache_within_advertised_bound", False)
                and m.get("boundedness", {}).get("recovered_within_5_seconds", False)
                for m in churn
            )
        )
        and (len(overload) >= rep)
        and all((m["recovery"]["restored_within_5_seconds"] for m in overload)),
        "agent_loop": len(result.get("agent_loops", [])) >= rep * len(required) * 3
        and all((not row["errors"] for row in result.get("agent_loops", []))),
        "workflow_budgets": len(result.get("workflow_budgets", [])) >= rep
        and all((not row["error"] for row in result.get("workflow_budgets", []))),
        "all_scenarios_selected": not result["not_run"],
    }


def archive_raw(output):
    files = sorted(output.glob("*.csv"))
    if not files:
        return None
    checksums = {}
    for path in files:
        with open(path, "rb") as stream:
            checksums[path.name] = {
                "bytes": path.stat().st_size,
                "sha256": hashlib.file_digest(stream, "sha256").hexdigest(),
            }
    temporary = output / "raw-samples.tar.gz.tmp"
    destination = output / "raw-samples.tar.gz"
    with tarfile.open(temporary, "w:gz", compresslevel=6) as archive:
        for path in files:
            archive.add(path, arcname=path.name, recursive=False)
    with tarfile.open(temporary, "r:gz") as archive:
        for name, expected in checksums.items():
            with archive.extractfile(name) as stream:
                if (
                    hashlib.file_digest(stream, "sha256").hexdigest()
                    != expected["sha256"]
                ):
                    raise RuntimeError("Archived sample checksum mismatch")
    temporary.replace(destination)
    (output / "raw-checksums.json").write_text(json.dumps(checksums, indent=2))
    for path in files:
        path.unlink()
    with open(destination, "rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    return {
        "path": destination.name,
        "sha256": digest,
        "files": len(files),
        "bytes": destination.stat().st_size,
        "checksum_manifest": "raw-checksums.json",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", choices=["smoke", "reference"], default="smoke")
    parser.add_argument(
        "--scenarios",
        default="read,batch,cold,agent-loop,mixed,churn,overload,workflow",
    )
    parser.add_argument("--seconds", type=float)
    parser.add_argument("--repetitions", type=int)
    parser.add_argument("--churn-seconds", type=float)
    parser.add_argument("--mixed-seconds", type=float)
    parser.add_argument("--aged-batch-followup", action="store_true")
    parser.add_argument("--clients", default="1,8,32")
    parser.add_argument("--backends", default="service,git_process,git_persistent")
    parser.add_argument("--arrival-rate", type=float, default=0)
    parser.add_argument(
        "--workdir", type=pathlib.Path, default=pathlib.Path("/tmp/aq-git-benchmark")
    )
    parser.add_argument("--output", type=pathlib.Path)
    parser.add_argument(
        "--binary", type=pathlib.Path, default=ROOT / "target/release/aq-git"
    )
    args = parser.parse_args()
    if args.seconds is not None and args.seconds <= 0:
        parser.error("seconds must be positive")
    if args.repetitions is not None and args.repetitions <= 0:
        parser.error("repetitions must be positive")
    if any(
        value is not None and value <= 0
        for value in (args.churn_seconds, args.mixed_seconds)
    ):
        parser.error("scenario durations must be positive")
    if args.arrival_rate < 0:
        parser.error("arrival rate must be nonnegative")
    if any((int(v) <= 0 or int(v) > 128 for v in args.clients.split(","))):
        parser.error("clients must be in 1..128")
    manifest = json.loads((ROOT / "benchmarks/fixtures/reference.json").read_text())
    reference = args.profile == "reference"
    seconds = args.seconds if args.seconds is not None else 60 if reference else 2
    repetitions = args.repetitions or (5 if reference else 1)
    churn_seconds = (
        args.churn_seconds
        if args.churn_seconds is not None
        else (1800 if reference and args.seconds is None else seconds)
    )
    mixed_seconds = args.mixed_seconds if args.mixed_seconds is not None else seconds
    commits = manifest["history"]["commits"] if reference else 2000
    refs = manifest["history"]["refs"] if reference else 200
    keys = manifest["workload"]["distinct_warm_keys"]
    clients = [int(v) for v in args.clients.split(",")]
    backends = tuple(args.backends.split(","))
    if not backends or any(
        backend not in ("service", "git_process", "git_persistent")
        for backend in backends
    ):
        parser.error("unknown backend")
    scenarios = args.scenarios.split(",")
    if any(
        (
            s
            not in (
                "read",
                "batch",
                "cold",
                "agent-loop",
                "mixed",
                "churn",
                "overload",
                "workflow",
            )
            for s in scenarios
        )
    ):
        parser.error("unknown scenario")
    timestamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
    output = args.output or ROOT / "benchmarks/results" / f"{timestamp}-{args.profile}"
    output.mkdir(parents=True, exist_ok=True)
    work = args.workdir.resolve()
    work.mkdir(parents=True, exist_ok=True)
    if not args.binary.exists():
        subprocess.run(
            ["cargo", "build", "--release", "-p", "aq-git-cli"], cwd=ROOT, check=True
        )
    paths = {
        str(uuid.uuid5(uuid.NAMESPACE_URL, f"aq-git-benchmark/{i}")): work
        / f"repository-{i}.git"
        for i in range(2)
    }
    for i, (repo, path) in enumerate(paths.items()):
        fixture(
            path,
            commits if i == 0 else max(2000, commits // 10),
            refs if i == 0 else max(200, refs // 10),
            manifest["seed"] + i,
        )
    workspace = work / "workspace-fixture"
    file_count = manifest["workspace"]["files"] if reference else 1000
    workspace_fixture(workspace, file_count)
    queries = make_queries(
        paths, keys, manifest["seed"], manifest["workload"]["max_entries"]
    )
    (output / "queries.json").write_text(json.dumps(queries, indent=2))
    state = work / f"state-{timestamp}"
    sock = work / f"socket-{os.getpid()}"
    log = open(output / "service.log", "wb")
    stall_remote = work / "stalled-remote.git"
    stall_release = work / f"stall-release-{timestamp}"
    stall_marker = work / f"stall-entered-{timestamp}"
    sync_enable = work / f"sync-enable-{timestamp}"
    sync_release = work / f"sync-release-{timestamp}"
    sync_marker = work / f"sync-entered-{timestamp}"
    if "mixed" in scenarios and (not stall_remote.exists()):
        git(
            work,
            ["clone", "--bare", str(next(iter(paths.values()))), str(stall_remote)],
        )
    wrapper = work / "git-barrier-wrapper"
    lines = ["#!/bin/sh", "for argument do"]
    for target, enable, marker, release_path in [
        (next(iter(paths.values())), sync_enable, sync_marker, sync_release),
        (stall_remote, None, stall_marker, stall_release),
    ]:
        condition = (
            '[ "$argument" = '
            + shlex.quote(str(target))
            + " ]"
            + (" && [ -e " + shlex.quote(str(enable)) + " ]" if enable else "")
        )
        lines += [
            "if " + condition + "; then",
            ": > " + shlex.quote(str(marker)),
            "while [ ! -e "
            + shlex.quote(str(release_path))
            + " ]; do /bin/sleep 0.01; done",
            "fi",
        ]
    lines += ["done", "exec " + shlex.quote(GIT) + ' "$@"']
    wrapper.write_text("\n".join(lines) + "\n")
    wrapper.chmod(448)
    service = start_server(args.binary, state, sock, log)
    result = {
        "format_version": 1,
        "profile": args.profile,
        "reference_sized_fixture": reference,
        "reference_acceptance_claim": False,
        "overrides": {
            "seconds": seconds,
            "repetitions": repetitions,
            "clients": clients,
            "backends": list(backends),
            "churn_seconds": churn_seconds,
            "mixed_seconds": mixed_seconds,
            "arrival_rate": args.arrival_rate,
            "aged_batch_followup": args.aged_batch_followup,
        },
        "manifest": manifest,
        "environment": {
            "platform": platform.platform(),
            "python": sys.version,
            "binary_sha256": hashlib.sha256(args.binary.read_bytes()).hexdigest(),
            "git_executable": GIT,
            "git": subprocess.check_output([GIT, "--version"]).decode().strip(),
            "rust": subprocess.check_output(["rustc", "--version"]).decode().strip(),
            "cpu_count": os.cpu_count(),
            "load_average": pathlib.Path("/proc/loadavg").read_text().strip(),
            "tokio_worker_threads_override": os.environ.get("TOKIO_WORKER_THREADS"),
            "cpu_model": next(
                (
                    line.split(":", 1)[1].strip()
                    for line in pathlib.Path("/proc/cpuinfo").read_text().splitlines()
                    if line.startswith("model name")
                ),
                "unknown",
            ),
            "cpu_affinity": sorted(os.sched_getaffinity(0)),
            "cgroup_cpu_controls": {
                str(p): p.read_text().strip()
                for p in (
                    pathlib.Path("/sys/fs/cgroup/cpu.max"),
                    pathlib.Path("/sys/fs/cgroup/cpu/cpu.cfs_quota_us"),
                    pathlib.Path("/sys/fs/cgroup/cpu/cpu.cfs_period_us"),
                )
                if p.exists()
            },
            "filesystem": subprocess.check_output(["stat", "-f", "-c", "%T", str(work)])
            .decode()
            .strip(),
        },
        "measurements": [],
        "checks": {},
        "not_run": [],
    }
    try:
        for _ in range(1000):
            if service.poll() is not None:
                raise RuntimeError("service exited; inspect service.log")
            if sock.exists():
                break
            time.sleep(0.01)
        control = Client(sock)
        for repo, path in paths.items():
            control.execute(
                request(
                    "repo.register",
                    {
                        "identity": "benchmark-" + repo,
                        "mode": "central",
                        "remote_url": str(path),
                        "allow_file_transport": True,
                        "allowed_refs": ["refs/heads/main", "refs/heads/left"],
                    },
                    repo,
                    True,
                    0,
                )
            )
            synchronized = control.execute(request("repo.sync", {}, repo, True))
            result.setdefault("initial_synchronizations", []).append(
                {"repository_id": repo, "observation": synchronized}
            )
        baseline = GitBaseline(paths, True)
        mismatches = []
        ancestry_counts = {True: 0, False: 0}
        t0 = time.perf_counter()
        for i, q in enumerate(queries):
            expected = baseline.execute(q)
            actual = control.execute(q)
            if q["operation"] == "contains":
                ancestry_counts[actual["contains"]] += 1
            if expected != actual:
                mismatches.append({"index": i, "expected": expected, "actual": actual})
        baseline.close()
        result["checks"]["correctness"] = {
            "queries": len(queries),
            "seconds": time.perf_counter() - t0,
            "mismatches": mismatches,
            "ancestry_true": ancestry_counts[True],
            "ancestry_false": ancestry_counts[False],
        }
        if mismatches:
            raise RuntimeError("service/baseline exact results differ")

        def metrics():
            try:
                return control.execute(request("service.metrics", {}))
            except Exception as e:
                return {"unavailable": str(e)}

        before = metrics()
        for q in queries:
            control.execute(q)
        after = metrics()
        result["checks"]["warm_git_work"] = {
            "before": before,
            "after": after,
            "equal": before.get("git") == after.get("git")
            and before.get("queries", {}).get("object_exchanges")
            == after.get("queries", {}).get("object_exchanges")
            and ("unavailable" not in before)
            and ("unavailable" not in after),
        }
        service, control = restart_server(
            service, control, args.binary, state, sock, log, wrapper
        )
        cold_seen = {
            json.dumps(
                [q["repository_id"], q["operation"], q["params"]], sort_keys=True
            )
            for q in queries
        }
        cold_one = next(
            (
                q
                for q in disjoint_queries(
                    paths, 100, manifest["seed"] + 999, 100, cold_seen
                )
                if q["operation"] == "inspect"
            )
        )
        before_burst = metrics()
        answers = burst(sock, [cold_one] * 32, output / "singleflight.json")
        after_burst = metrics()
        exchanges = (
            after_burst["queries"]["object_exchanges"]
            - before_burst["queries"]["object_exchanges"]
        )
        result["checks"]["singleflight"] = {
            "object_exchanges": exchanges,
            "errors": [row["error"] for row in answers if row["error"]],
            "pass": exchanges == 1 and all((not row["error"] for row in answers)),
        }
        first_repo = next(iter(paths))
        sync_requests = [request("repo.sync", {}, first_repo, True) for _ in range(32)]
        barrier_evidence = {"all_admitted_before_release": False}
        sync_enable.touch()

        def release_sync_burst():
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline:
                durable = 0
                for sync_request in sync_requests:
                    try:
                        control.execute(
                            request(
                                "operation.show",
                                {"target_operation_id": sync_request["operation_id"]},
                            )
                        )
                        durable += 1
                    except RuntimeError:
                        pass
                if durable == 32 and sync_marker.exists():
                    barrier_evidence["all_admitted_before_release"] = True
                    break
                time.sleep(0.005)
            sync_release.touch()

        before_sync = metrics()
        answers = burst(
            sock, sync_requests, output / "shared-sync.json", release_sync_burst
        )
        sync_enable.unlink(missing_ok=True)
        after_sync = metrics()
        spawns = after_sync["git"]["spawns"] - before_sync["git"]["spawns"]
        identities = {
            row["result"]["observation_id"] for row in answers if row["result"]
        }
        result["checks"]["shared_sync"] = {
            "git_spawns": spawns,
            "observations": len(identities),
            "errors": [row["error"] for row in answers if row["error"]],
            **barrier_evidence,
            "pass": spawns == 4
            and len(identities) == 1
            and all((not row["error"] for row in answers))
            and barrier_evidence["all_admitted_before_release"],
        }
        service, control = restart_server(
            service, control, args.binary, state, sock, log
        )
        for warm_query in queries:
            control.execute(warm_query)
        before = metrics()
        for warm_query in queries:
            control.execute(warm_query)
        after = metrics()
        result["checks"]["initial_warm_git_work"] = result["checks"]["warm_git_work"]
        result["checks"]["warm_git_work"] = {
            "before": before,
            "after": after,
            "equal": before.get("git") == after.get("git")
            and before["queries"]["object_exchanges"]
            == after["queries"]["object_exchanges"],
            "service_pid": service.pid,
        }
        result["capabilities"] = control.execute(request("capabilities", {}))
        result["environment"]["advertised_socket_io_workers"] = result["capabilities"][
            "limits"
        ].get("socket_io_workers")
        result["configuration_overrides"] = {
            "cache_bytes": result["capabilities"]["limits"]["cache_bytes"],
            "cache_entries": result["capabilities"]["limits"]["cache_entries"],
            "manifest_cache_bytes": manifest["workload"]["cache_bytes"],
            "note": "Actual advertised implementation budget is declared explicitly; the working set must fit this budget.",
        }
        for scenario in scenarios:
            if scenario == "workflow":
                for repetition in range(repetitions):
                    row = workflow_budgets(control, workspace, work, file_count)
                    row["repetition"] = repetition + 1
                    result.setdefault("workflow_budgets", []).append(row)
                continue
            if scenario == "agent-loop":
                for repetition in range(repetitions):
                    for count in clients:
                        for kind in backends:
                            subdirectory = output / f"agent-loop-{repetition + 1}"
                            subdirectory.mkdir(exist_ok=True)
                            row = agent_loop(
                                control, sock, paths, count, seconds, kind, subdirectory
                            )
                            row["repetition"] = repetition + 1
                            result.setdefault("agent_loops", []).append(row)
                continue
            if scenario == "overload":
                for repetition in range(repetitions):
                    measurement = measure(
                        "service",
                        queries,
                        paths,
                        sock,
                        96,
                        seconds,
                        manifest["seed"],
                        output / f"overload-{repetition}",
                        service_pid=service.pid,
                    )
                    measurement.update(
                        scenario="overload",
                        repetition=repetition + 1,
                        clients=96,
                        backend="service",
                    )
                    rewarm_start = time.perf_counter()
                    for q in queries:
                        control.execute(q)
                    rewarm_ms = (time.perf_counter() - rewarm_start) * 1000
                    recovery = measure(
                        "service",
                        queries,
                        paths,
                        sock,
                        max(clients),
                        min(5, seconds),
                        manifest["seed"],
                        output / f"overload-recovery-{repetition}",
                        service_pid=service.pid,
                    )
                    measurement["recovery"] = {
                        "rewarm_ms": rewarm_ms,
                        "measurement": recovery,
                        "restored_within_5_seconds": rewarm_ms <= 5000
                        and recovery["p95_ms"] <= 5
                        and (recovery["p99_ms"] <= 20),
                    }
                    result["measurements"].append(measurement)
                continue
            if scenario == "mixed":
                service, control = restart_server(
                    service, control, args.binary, state, sock, log, wrapper
                )
                for warm_query in queries:
                    control.execute(warm_query)
                for repetition in range(repetitions):
                    stall_release.unlink(missing_ok=True)
                    stall_marker.unlink(missing_ok=True)
                    count = max(clients)
                    matched_control = measure(
                        "service",
                        queries,
                        paths,
                        sock,
                        count,
                        mixed_seconds,
                        manifest["seed"],
                        output / f"mixed-control-{repetition + 1}",
                        service_pid=service.pid,
                    )
                    background = mixed_setup(
                        control,
                        sock,
                        paths,
                        state,
                        work,
                        args.binary,
                        log,
                        stall_remote,
                        stall_release,
                        stall_marker,
                    )
                    measurement = measure(
                        "service",
                        queries,
                        paths,
                        sock,
                        count,
                        mixed_seconds,
                        manifest["seed"],
                        output / f"mixed-{repetition + 1}",
                        background=background,
                        service_pid=service.pid,
                    )
                    measurement.update(
                        scenario="mixed",
                        repetition=repetition + 1,
                        clients=count,
                        backend="service",
                        matched_read_control=matched_control,
                    )
                    result["measurements"].append(measurement)
                service, control = restart_server(
                    service, control, args.binary, state, sock, log
                )
                for warm_query in queries:
                    control.execute(warm_query)
                continue
            if scenario not in (
                "read",
                "batch",
                "churn",
                "cold",
                "agent-loop",
                "mixed",
                "overload",
            ):
                result["not_run"].append(
                    {
                        "scenario": scenario,
                        "reason": "Heavy-operation/agent-loop orchestration has not been implemented; this is not acceptance evidence.",
                    }
                )
                continue
            current_queries = queries
            if scenario == "churn":
                current_queries = make_queries(
                    paths, 20000 if reference else 6000, manifest["seed"] + 1, 100
                )
            for repetition in range(repetitions):
                for count in [max(clients)] if scenario == "churn" else clients:
                    if scenario == "cold":
                        current_queries = disjoint_queries(
                            paths,
                            keys,
                            manifest["seed"] + 10000 + repetition * 100 + count,
                            100,
                            cold_seen,
                        )
                    if scenario == "cold":
                        (
                            output / f"cold-{repetition + 1}-{count}-queries.json"
                        ).write_text(json.dumps(current_queries, indent=2))
                    for kind in ("service",) if scenario == "churn" else backends:
                        print(
                            f"{scenario} repetition={repetition + 1} clients={count} backend={kind}",
                            flush=True,
                        )
                        name = f"{scenario}-{repetition + 1}-{count}-{kind}"
                        before_measurement = metrics()
                        measurement = measure(
                            kind,
                            current_queries,
                            paths,
                            sock,
                            count,
                            churn_seconds if scenario == "churn" else seconds,
                            manifest["seed"] + repetition,
                            output / name,
                            args.arrival_rate,
                            scenario == "batch",
                            scenario == "cold",
                            service_pid=service.pid,
                        )
                        measurement.update(
                            scenario=scenario,
                            repetition=repetition + 1,
                            clients=count,
                            backend=kind,
                        )
                        result["measurements"].append(measurement)
                        if (
                            args.aged_batch_followup
                            and scenario == "batch"
                            and kind == "service"
                            and not measurement["error_count"]
                            and measurement["p95_ms"] <= 25
                            and measurement["p99_ms"] <= 100
                        ):
                            print("aged same-service batch follow-up", flush=True)
                            followup = measure(
                                kind,
                                current_queries,
                                paths,
                                sock,
                                count,
                                seconds,
                                manifest["seed"] + repetition,
                                output / (name + "-aged-followup"),
                                args.arrival_rate,
                                batch=True,
                                service_pid=service.pid,
                            )
                            followup.update(
                                scenario="batch",
                                clients=count,
                                backend=kind,
                                repetition=repetition + 1,
                                same_aged_service_followup=True,
                            )
                            result.setdefault("aged_batch_followups", []).append(
                                followup
                            )
                        result["service_process_tree"] = process_tree(service.pid)
                        if scenario == "churn" and kind == "service":
                            bounds = metrics()
                            rewarm_start = time.perf_counter()
                            for warm_query in queries:
                                control.execute(warm_query)
                            rewarm_ms = (time.perf_counter() - rewarm_start) * 1000
                            recovery = measure(
                                "service",
                                queries,
                                paths,
                                sock,
                                count,
                                min(5, seconds),
                                manifest["seed"],
                                output / (name + "-recovery"),
                                service_pid=service.pid,
                            )
                            measurement["boundedness"] = {
                                "metrics": bounds,
                                "evictions_observed": bounds.get("queries", {}).get(
                                    "cache_evictions", 0
                                )
                                > before_measurement.get("queries", {}).get(
                                    "cache_evictions", 0
                                ),
                                "cache_within_advertised_bound": bounds.get(
                                    "queries", {}
                                ).get("cache_bytes", 2**64)
                                <= 16 * 1024 * 1024
                                and bounds.get("queries", {}).get(
                                    "cache_entries", 2**64
                                )
                                <= 4096,
                                "rewarm_ms": rewarm_ms,
                                "recovery": recovery,
                                "recovered_within_5_seconds": rewarm_ms <= 5000
                                and recovery["p95_ms"] <= 5
                                and (recovery["p99_ms"] <= 20),
                            }
                        (output / "results.json").write_text(
                            json.dumps(result, indent=2)
                        )
        targets = manifest["targets"]
        gates = []
        for m in result["measurements"]:
            if m["backend"] != "service":
                continue
            threshold95 = (
                targets["batch_p95_ms"]
                if m["scenario"] == "batch"
                else targets["warm_p95_ms"]
            )
            threshold99 = (
                targets["batch_p99_ms"]
                if m["scenario"] == "batch"
                else targets["mixed_p99_ms"]
                if m["scenario"] == "mixed"
                else targets["warm_p99_ms"]
            )
            gate = {
                "scenario": m["scenario"],
                "repetition": m["repetition"],
                "clients": m["clients"],
                "latency_pass": m["p95_ms"] is not None
                and m["p95_ms"] <= threshold95
                and (m["p99_ms"] <= threshold99),
                "zero_errors": not m["errors"],
            }
            peer = next(
                (
                    p
                    for p in result["measurements"]
                    if p["backend"] == "git_process"
                    and all(
                        (p[k] == m[k] for k in ("scenario", "repetition", "clients"))
                    )
                ),
                None,
            )
            if peer and m["p95_ms"] and peer["queries_per_second"]:
                gate.update(
                    p95_speedup=peer["p95_ms"] / m["p95_ms"],
                    throughput_speedup=m["queries_per_second"]
                    / peer["queries_per_second"],
                )
            if peer:
                gate["classes"] = {
                    operation: {
                        "p95_speedup": peer["classes"][operation]["p95_ms"]
                        / values["p95_ms"],
                        "throughput_speedup": values["queries_per_second"]
                        / peer["classes"][operation]["queries_per_second"],
                    }
                    for operation, values in m["classes"].items()
                    if operation in peer["classes"]
                    and values["p95_ms"]
                    and peer["classes"][operation]["queries_per_second"]
                }
            if m["scenario"] == "cold" and peer:
                gate["p6_classes"] = {
                    op: {
                        "service_p95_ms": values["p95_ms"],
                        "process_git_p95_ms": peer["classes"][op]["p95_ms"],
                        "pass": values["p95_ms"]
                        <= peer["classes"][op]["p95_ms"] * 2 + 2,
                    }
                    for op, values in m["classes"].items()
                    if op in peer["classes"]
                }
                gate["cold_answer_agreement"] = cold_answer_agreement(m, peer)
                gate["p6_pass"] = (
                    gate["cold_answer_agreement"]["pass"]
                    and gate["zero_errors"]
                    and m.get("finite_workload", {}).get("complete", False)
                    and peer.get("finite_workload", {}).get("complete", False)
                    and all((v["pass"] for v in gate["p6_classes"].values()))
                )
            if m["scenario"] == "mixed":
                read_peer = m.get("matched_read_control")
                gate["p5_numerical_diagnostic_pass"] = bool(
                    read_peer
                    and m["p99_ms"] <= 50
                    and (
                        m["logical_queries_per_second"]
                        >= read_peer["logical_queries_per_second"] * 0.8
                    )
                    and m["background"]["all_four_classes_progressed"]
                    and m["background"]["four_way_overlap_seconds"] > 0
                    and all(
                        not event["error"] for event in m["background"]["operations"]
                    )
                    and not m["background"]["cleanup_errors"]
                    and (not m["errors"])
                )
                gate["p5_pass"] = (
                    gate["p5_numerical_diagnostic_pass"] and m["seconds"] >= 60
                )
                gate["read_throughput_ratio"] = (
                    m["logical_queries_per_second"]
                    / read_peer["logical_queries_per_second"]
                    if read_peer
                    else None
                )
            if m["scenario"] == "overload":
                gate["overload_recovered"] = m["recovery"]["restored_within_5_seconds"]
            if m["scenario"] == "churn":
                gate["p7_partial_pass"] = (
                    m.get("boundedness", {}).get("evictions_observed", False)
                    and m.get("boundedness", {}).get(
                        "cache_within_advertised_bound", False
                    )
                    and m.get("boundedness", {}).get(
                        "recovered_within_5_seconds", False
                    )
                )
            if m["clients"] == 32 and m["scenario"] == "read":
                gate.update(
                    p2_pass=gate.get("p95_speedup", 0) >= 10
                    and gate.get("throughput_speedup", 0) >= 5,
                    p3_numerical_diagnostic_pass=m["queries_per_second"] >= 1000
                    and gate["latency_pass"]
                    and gate["zero_errors"],
                )
                gate["p3_pass"] = (
                    gate["p3_numerical_diagnostic_pass"] and m["seconds"] >= 60
                )
            gates.append(gate)
        result["gates"] = gates
        result["checks"]["final_metrics"] = metrics()
        control.close()
        result["not_run"] += [
            {"scenario": s, "reason": "Not selected; no acceptance claim."}
            for s in (
                "read",
                "batch",
                "mixed",
                "agent-loop",
                "cold",
                "churn",
                "overload",
                "workflow",
            )
            if s not in scenarios
        ]
        result["acceptance_matrix"] = acceptance_matrix(result)
        result["reference_acceptance_claim"] = all(result["acceptance_matrix"].values())
    finally:
        result["run_status"] = "failed" if sys.exc_info()[0] else "complete"
        if sys.exc_info()[1]:
            result["failure"] = str(sys.exc_info()[1])
        if wrapper:
            stall_release.touch()
            sync_release.touch()
        for extra in SERVERS:
            if extra.pid != service.pid and extra.poll() is None:
                extra.send_signal(signal.SIGTERM)
                try:
                    extra.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    extra.kill()
                    extra.wait()
        service.send_signal(signal.SIGTERM)
        try:
            service.wait(timeout=10)
        except subprocess.TimeoutExpired:
            service.kill()
            service.wait()
        log.close()
        result["service_exit_code"] = service.returncode
        result["raw_sample_archive"] = archive_raw(output)
        (output / "results.json").write_text(json.dumps(result, indent=2))
    print(output)
    print(json.dumps(result.get("gates", []), indent=2))
    return (
        1
        if (
            result["checks"].get("correctness", {}).get("mismatches")
            or any(
                not gate["cold_answer_agreement"]["pass"]
                for gate in result.get("gates", [])
                if "cold_answer_agreement" in gate
            )
        )
        else 0
    )


if __name__ == "__main__":
    sys.exit(main())
