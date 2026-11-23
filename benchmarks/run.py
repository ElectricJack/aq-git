#!/usr/bin/env python3
"""Reproducible standalone Git/socket benchmark. No third party Python packages."""

import hashlib
import json
import os
import pathlib
import platform
import shlex
import signal
import subprocess
import sys
import time
import uuid

# The driver is also loaded by filename by the standalone profiling tools.
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
try:
    from aq_git_bench.agent_loop import AgentLoop
    from aq_git_bench.archive import archive_raw
    from aq_git_bench.baseline import GitBaseline
    from aq_git_bench.checks import (
        check_correctness,
        check_shared_sync,
        check_singleflight,
        check_warm_reads,
        warm_restarted_service,
    )
    from aq_git_bench.config import ROOT, BenchmarkPlan, parse_args
    from aq_git_bench.fixtures import (
        disjoint_queries,
        fixture,
        make_queries,
        workspace_fixture,
    )
    from aq_git_bench.gates import (
        acceptance_matrix,
        measurement_gates,
    )
    from aq_git_bench.gates import (
        cold_answer_agreement as cold_answer_agreement,
    )
    from aq_git_bench.measurement import Measurement, Workload
    from aq_git_bench.mixed import MixedBackground
    from aq_git_bench.protocol import GIT, Client, envelope, git
    from aq_git_bench.resources import process_tree
    from aq_git_bench.service import (
        SERVERS,
        ServerLocation,
    )
    from aq_git_bench.workflow import workflow_budgets
finally:
    sys.path.pop(0)


def request(op, params, repo=None, mutation=False, generation=1):
    return envelope(op, params, (repo, generation), mutation)


def start_server(binary, state, sock, log, git_executable=None):
    return ServerLocation(binary, state, sock, log).start(git_executable)


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
    workload = Workload(
        kind,
        queries,
        paths,
        sock,
        clients,
        seconds,
        seed,
        raw_path,
        arrival_rate,
        batch,
        unique,
        unique_budget_seconds,
    )
    return Measurement(workload, GitBaseline).run(service_pid, background)


class BenchmarkRun:
    def __init__(self, plan):
        self.plan = plan
        self.prepare_fixtures()
        self.prepare_sockets()
        self.start_service()
        self.initialize_report()

    def prepare_fixtures(self):
        self.timestamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
        self.output = (
            self.plan.args.output
            or ROOT
            / "benchmarks/results"
            / f"{self.timestamp}-{self.plan.args.profile}"
        )
        self.output.mkdir(parents=True, exist_ok=True)
        self.work = self.plan.args.workdir.resolve()
        self.work.mkdir(parents=True, exist_ok=True)
        if not self.plan.args.binary.exists():
            subprocess.run(
                ["cargo", "build", "--release", "-p", "aq-git-cli"],
                cwd=ROOT,
                check=True,
            )
        self.paths = {
            str(uuid.uuid5(uuid.NAMESPACE_URL, f"aq-git-benchmark/{i}")): self.work
            / f"repository-{i}.git"
            for i in range(2)
        }
        for i, (repo, path) in enumerate(self.paths.items()):
            fixture(
                path,
                self.plan.commits if i == 0 else max(2000, self.plan.commits // 10),
                self.plan.refs if i == 0 else max(200, self.plan.refs // 10),
                self.plan.manifest["seed"] + i,
            )
        self.workspace = self.work / "workspace-fixture"
        self.file_count = (
            self.plan.manifest["workspace"]["files"] if self.plan.reference else 1000
        )
        workspace_fixture(self.workspace, self.file_count)
        self.queries = make_queries(
            self.paths,
            self.plan.keys,
            self.plan.manifest["seed"],
            self.plan.manifest["workload"]["max_entries"],
        )
        (self.output / "queries.json").write_text(json.dumps(self.queries, indent=2))

    def prepare_sockets(self):
        self.state = self.work / f"state-{self.timestamp}"
        self.sock = self.work / f"socket-{os.getpid()}"
        self.log = open(self.output / "service.log", "wb")
        self.stall_remote = self.work / "stalled-remote.git"
        self.stall_release = self.work / f"stall-release-{self.timestamp}"
        self.stall_marker = self.work / f"stall-entered-{self.timestamp}"
        self.sync_enable = self.work / f"sync-enable-{self.timestamp}"
        self.sync_release = self.work / f"sync-release-{self.timestamp}"
        self.sync_marker = self.work / f"sync-entered-{self.timestamp}"
        if "mixed" in self.plan.scenarios and (not self.stall_remote.exists()):
            git(
                self.work,
                [
                    "clone",
                    "--bare",
                    str(next(iter(self.paths.values()))),
                    str(self.stall_remote),
                ],
            )
        self.wrapper = self.work / "git-barrier-wrapper"
        lines = ["#!/bin/sh", "for argument do"]
        for target, enable, marker, release_path in [
            (
                next(iter(self.paths.values())),
                self.sync_enable,
                self.sync_marker,
                self.sync_release,
            ),
            (self.stall_remote, None, self.stall_marker, self.stall_release),
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
        self.wrapper.write_text("\n".join(lines) + "\n")
        self.wrapper.chmod(448)

    def start_service(self):
        self.endpoint = ServerLocation(
            self.plan.args.binary, self.state, self.sock, self.log
        )
        self.service = self.endpoint.start()

    def initialize_report(self):
        self.result = {
            "format_version": 1,
            "profile": self.plan.args.profile,
            "reference_sized_fixture": self.plan.reference,
            "reference_acceptance_claim": False,
            "overrides": {
                "seconds": self.plan.seconds,
                "repetitions": self.plan.repetitions,
                "clients": self.plan.clients,
                "backends": list(self.plan.backends),
                "churn_seconds": self.plan.churn_seconds,
                "mixed_seconds": self.plan.mixed_seconds,
                "arrival_rate": self.plan.args.arrival_rate,
                "aged_batch_followup": self.plan.args.aged_batch_followup,
            },
            "manifest": self.plan.manifest,
            "environment": {
                "platform": platform.platform(),
                "python": sys.version,
                "binary_sha256": hashlib.sha256(
                    self.plan.args.binary.read_bytes()
                ).hexdigest(),
                "git_executable": GIT,
                "git": subprocess.check_output([GIT, "--version"]).decode().strip(),
                "rust": subprocess.check_output(["rustc", "--version"])
                .decode()
                .strip(),
                "cpu_count": os.cpu_count(),
                "load_average": pathlib.Path("/proc/loadavg").read_text().strip(),
                "tokio_worker_threads_override": os.environ.get("TOKIO_WORKER_THREADS"),
                "cpu_model": next(
                    (
                        line.split(":", 1)[1].strip()
                        for line in pathlib.Path("/proc/cpuinfo")
                        .read_text()
                        .splitlines()
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
                "filesystem": subprocess.check_output(
                    ["stat", "-f", "-c", "%T", str(self.work)]
                )
                .decode()
                .strip(),
            },
            "measurements": [],
            "checks": {},
            "not_run": [],
        }

    def register(self):
        for _ in range(1000):
            if self.service.poll() is not None:
                raise RuntimeError("service exited; inspect service.log")
            if self.sock.exists():
                break
            time.sleep(0.01)
        self.control = Client(self.sock)
        for repo, path in self.paths.items():
            self.control.execute(
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
            synchronized = self.control.execute(request("repo.sync", {}, repo, True))
            self.result.setdefault("initial_synchronizations", []).append(
                {"repository_id": repo, "observation": synchronized}
            )

    def metrics(self):
        try:
            return self.control.execute(request("service.metrics", {}))
        except Exception as e:
            return {"unavailable": str(e)}

    def workflow(self):
        for repetition in range(self.plan.repetitions):
            row = workflow_budgets(
                self.control, self.workspace, self.work, self.file_count
            )
            row["repetition"] = repetition + 1
            self.result.setdefault("workflow_budgets", []).append(row)

    def agent_scenario(self):
        for repetition in range(self.plan.repetitions):
            for count in self.plan.clients:
                for kind in self.plan.backends:
                    subdirectory = self.output / f"agent-loop-{repetition + 1}"
                    subdirectory.mkdir(exist_ok=True)
                    row = AgentLoop(self, kind, count).measure(subdirectory)
                    row["repetition"] = repetition + 1
                    self.result.setdefault("agent_loops", []).append(row)

    def overload(self):
        for repetition in range(self.plan.repetitions):
            measurement = measure(
                "service",
                self.queries,
                self.paths,
                self.sock,
                96,
                self.plan.seconds,
                self.plan.manifest["seed"],
                self.output / f"overload-{repetition}",
                service_pid=self.service.pid,
            )
            measurement.update(
                scenario="overload",
                repetition=repetition + 1,
                clients=96,
                backend="service",
            )
            rewarm_start = time.perf_counter()
            for q in self.queries:
                self.control.execute(q)
            rewarm_ms = (time.perf_counter() - rewarm_start) * 1000
            recovery = measure(
                "service",
                self.queries,
                self.paths,
                self.sock,
                max(self.plan.clients),
                min(5, self.plan.seconds),
                self.plan.manifest["seed"],
                self.output / f"overload-recovery-{repetition}",
                service_pid=self.service.pid,
            )
            measurement["recovery"] = {
                "rewarm_ms": rewarm_ms,
                "measurement": recovery,
                "restored_within_5_seconds": rewarm_ms <= 5000
                and recovery["p95_ms"] <= 5
                and (recovery["p99_ms"] <= 20),
            }
            self.result["measurements"].append(measurement)

    def mixed(self):
        self.service, self.control = self.endpoint.restart(
            self.service,
            self.control,
            self.wrapper,
        )
        for warm_query in self.queries:
            self.control.execute(warm_query)
        for repetition in range(self.plan.repetitions):
            self.stall_release.unlink(missing_ok=True)
            self.stall_marker.unlink(missing_ok=True)
            count = max(self.plan.clients)
            matched_control = measure(
                "service",
                self.queries,
                self.paths,
                self.sock,
                count,
                self.plan.mixed_seconds,
                self.plan.manifest["seed"],
                self.output / f"mixed-control-{repetition + 1}",
                service_pid=self.service.pid,
            )
            background = MixedBackground(self)
            measurement = measure(
                "service",
                self.queries,
                self.paths,
                self.sock,
                count,
                self.plan.mixed_seconds,
                self.plan.manifest["seed"],
                self.output / f"mixed-{repetition + 1}",
                background=background,
                service_pid=self.service.pid,
            )
            measurement.update(
                scenario="mixed",
                repetition=repetition + 1,
                clients=count,
                backend="service",
                matched_read_control=matched_control,
            )
            self.result["measurements"].append(measurement)
        self.service, self.control = self.endpoint.restart(
            self.service,
            self.control,
        )
        for warm_query in self.queries:
            self.control.execute(warm_query)

    def queries_scenario(self, scenario):
        current_queries = self.queries
        if scenario == "churn":
            current_queries = make_queries(
                self.paths,
                20000 if self.plan.reference else 6000,
                self.plan.manifest["seed"] + 1,
                100,
            )
        for repetition in range(self.plan.repetitions):
            for count in (
                [max(self.plan.clients)] if scenario == "churn" else self.plan.clients
            ):
                if scenario == "cold":
                    current_queries = disjoint_queries(
                        self.paths,
                        self.plan.keys,
                        self.plan.manifest["seed"] + 10000 + repetition * 100 + count,
                        self.cold_seen,
                    )
                if scenario == "cold":
                    (
                        self.output / f"cold-{repetition + 1}-{count}-queries.json"
                    ).write_text(json.dumps(current_queries, indent=2))
                for kind in ("service",) if scenario == "churn" else self.plan.backends:
                    self.measure_backend(
                        (scenario, repetition, count, kind), current_queries
                    )

    def measure_backend(self, case, current_queries):
        scenario, repetition, count, kind = case
        print(
            f"{scenario} repetition={repetition + 1} clients={count} backend={kind}",
            flush=True,
        )
        name = f"{scenario}-{repetition + 1}-{count}-{kind}"
        before_measurement = self.metrics()
        measurement = measure(
            kind,
            current_queries,
            self.paths,
            self.sock,
            count,
            self.plan.churn_seconds if scenario == "churn" else self.plan.seconds,
            self.plan.manifest["seed"] + repetition,
            self.output / name,
            self.plan.args.arrival_rate,
            scenario == "batch",
            scenario == "cold",
            service_pid=self.service.pid,
        )
        measurement.update(
            scenario=scenario,
            repetition=repetition + 1,
            clients=count,
            backend=kind,
        )
        self.result["measurements"].append(measurement)
        if self.should_repeat_batch(scenario, kind, measurement):
            self.repeat_batch(case, current_queries, name)
        self.result["service_process_tree"] = process_tree(self.service.pid)
        if scenario == "churn" and kind == "service":
            self.check_cache_recovery(count, name, (before_measurement, measurement))
        (self.output / "results.json").write_text(json.dumps(self.result, indent=2))

    def should_repeat_batch(self, scenario, kind, measurement):
        return (
            self.plan.args.aged_batch_followup
            and (scenario, kind) == ("batch", "service")
            and not measurement["error_count"]
            and measurement["p95_ms"] <= 25
            and measurement["p99_ms"] <= 100
        )

    def repeat_batch(self, case, current_queries, name):
        scenario, repetition, count, kind = case
        print("aged same-service batch follow-up", flush=True)
        followup = measure(
            kind,
            current_queries,
            self.paths,
            self.sock,
            count,
            self.plan.seconds,
            self.plan.manifest["seed"] + repetition,
            self.output / (name + "-aged-followup"),
            self.plan.args.arrival_rate,
            batch=True,
            service_pid=self.service.pid,
        )
        followup.update(
            scenario="batch",
            clients=count,
            backend=kind,
            repetition=repetition + 1,
            same_aged_service_followup=True,
        )
        self.result.setdefault("aged_batch_followups", []).append(followup)

    def check_cache_recovery(self, count, name, outcome):
        before_measurement, measurement = outcome
        bounds = self.metrics()
        rewarm_start = time.perf_counter()
        for warm_query in self.queries:
            self.control.execute(warm_query)
        rewarm_ms = (time.perf_counter() - rewarm_start) * 1000
        recovery = measure(
            "service",
            self.queries,
            self.paths,
            self.sock,
            count,
            min(5, self.plan.seconds),
            self.plan.manifest["seed"],
            self.output / (name + "-recovery"),
            service_pid=self.service.pid,
        )
        measurement["boundedness"] = {
            "metrics": bounds,
            "evictions_observed": bounds.get("queries", {}).get("cache_evictions", 0)
            > before_measurement.get("queries", {}).get("cache_evictions", 0),
            "cache_within_advertised_bound": bounds.get("queries", {}).get(
                "cache_bytes", 2**64
            )
            <= 16 * 1024 * 1024
            and bounds.get("queries", {}).get("cache_entries", 2**64) <= 4096,
            "rewarm_ms": rewarm_ms,
            "recovery": recovery,
            "recovered_within_5_seconds": rewarm_ms <= 5000
            and recovery["p95_ms"] <= 5
            and (recovery["p99_ms"] <= 20),
        }

    def finish_report(self):
        self.result["gates"] = measurement_gates(
            self.result, self.plan.manifest["targets"]
        )
        self.result["checks"]["final_metrics"] = self.metrics()
        self.control.close()
        self.result["not_run"] += [
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
            if s not in self.plan.scenarios
        ]
        self.result["acceptance_matrix"] = acceptance_matrix(self.result)
        self.result["reference_acceptance_claim"] = all(
            self.result["acceptance_matrix"].values()
        )

    def stop(self):
        self.result["run_status"] = "failed" if sys.exc_info()[0] else "complete"
        if sys.exc_info()[1]:
            self.result["failure"] = str(sys.exc_info()[1])
        if self.wrapper:
            self.stall_release.touch()
            self.sync_release.touch()
        for extra in SERVERS:
            if extra.pid != self.service.pid and extra.poll() is None:
                extra.send_signal(signal.SIGTERM)
                try:
                    extra.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    extra.kill()
                    extra.wait()
        self.service.send_signal(signal.SIGTERM)
        try:
            self.service.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.service.kill()
            self.service.wait()
        self.log.close()
        self.result["service_exit_code"] = self.service.returncode
        self.result["raw_sample_archive"] = archive_raw(self.output)
        (self.output / "results.json").write_text(json.dumps(self.result, indent=2))

    def exit_status(self):
        print(self.output)
        print(json.dumps(self.result.get("gates", []), indent=2))
        return (
            1
            if (
                self.result["checks"].get("correctness", {}).get("mismatches")
                or any(
                    not gate["cold_answer_agreement"]["pass"]
                    for gate in self.result.get("gates", [])
                    if "cold_answer_agreement" in gate
                )
            )
            else 0
        )

    def execute(self):
        try:
            self.register()
            check_correctness(self)
            check_warm_reads(self)
            check_singleflight(self)
            check_shared_sync(self)
            warm_restarted_service(self)
            handlers = {
                "workflow": self.workflow,
                "agent-loop": self.agent_scenario,
                "overload": self.overload,
                "mixed": self.mixed,
            }
            for scenario in self.plan.scenarios:
                if scenario in handlers:
                    handlers[scenario]()
                else:
                    self.queries_scenario(scenario)
            self.finish_report()
        finally:
            self.stop()
        return self.exit_status()


def main():
    args, parser = parse_args(__doc__)
    return BenchmarkRun(BenchmarkPlan(args, parser)).execute()


if __name__ == "__main__":
    sys.exit(main())
