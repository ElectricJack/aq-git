import json
import multiprocessing
import time

from .baseline import GitBaseline
from .fixtures import disjoint_queries
from .protocol import Client, request


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


def check_correctness(run):
    baseline = GitBaseline(run.paths, True)
    mismatches = []
    ancestry_counts = {True: 0, False: 0}
    t0 = time.perf_counter()
    for i, q in enumerate(run.queries):
        expected = baseline.execute(q)
        actual = run.control.execute(q)
        if q["operation"] == "contains":
            ancestry_counts[actual["contains"]] += 1
        if expected != actual:
            mismatches.append({"index": i, "expected": expected, "actual": actual})
    baseline.close()
    run.result["checks"]["correctness"] = {
        "queries": len(run.queries),
        "seconds": time.perf_counter() - t0,
        "mismatches": mismatches,
        "ancestry_true": ancestry_counts[True],
        "ancestry_false": ancestry_counts[False],
    }
    if mismatches:
        raise RuntimeError("service/baseline exact results differ")


def warm_queries(run):
    for query in run.queries:
        run.control.execute(query)


def check_warm_reads(run):
    before = run.metrics()
    warm_queries(run)
    after = run.metrics()
    run.result["checks"]["warm_git_work"] = {
        "before": before,
        "after": after,
        "equal": before.get("git") == after.get("git")
        and before.get("queries", {}).get("object_exchanges")
        == after.get("queries", {}).get("object_exchanges")
        and ("unavailable" not in before)
        and ("unavailable" not in after),
    }


def check_singleflight(run):
    run.service, run.control = run.endpoint.restart(
        run.service,
        run.control,
        run.wrapper,
    )
    run.cold_seen = {
        json.dumps([q["repository_id"], q["operation"], q["params"]], sort_keys=True)
        for q in run.queries
    }
    cold_one = next(
        (
            q
            for q in disjoint_queries(
                run.paths,
                100,
                run.plan.manifest["seed"] + 999,
                run.cold_seen,
            )
            if q["operation"] == "inspect"
        )
    )
    before_burst = run.metrics()
    answers = burst(run.sock, [cold_one] * 32, run.output / "singleflight.json")
    after_burst = run.metrics()
    exchanges = (
        after_burst["queries"]["object_exchanges"]
        - before_burst["queries"]["object_exchanges"]
    )
    errors = burst_errors(answers)
    run.result["checks"]["singleflight"] = {
        "object_exchanges": exchanges,
        "errors": errors,
        "pass": exchanges == 1 and not errors,
    }


def check_shared_sync(run):
    first_repo = next(iter(run.paths))
    sync_requests = [request("repo.sync", {}, first_repo, True) for _ in range(32)]
    barrier_evidence = {"all_admitted_before_release": False}
    run.sync_enable.touch()

    def release_sync_burst():
        barrier_evidence["all_admitted_before_release"] = wait_for_admission(
            run, sync_requests
        )
        run.sync_release.touch()

    before_sync = run.metrics()
    answers = burst(
        run.sock,
        sync_requests,
        run.output / "shared-sync.json",
        release_sync_burst,
    )
    run.sync_enable.unlink(missing_ok=True)
    after_sync = run.metrics()
    spawns = after_sync["git"]["spawns"] - before_sync["git"]["spawns"]
    identities = {row["result"]["observation_id"] for row in answers if row["result"]}
    errors = burst_errors(answers)
    run.result["checks"]["shared_sync"] = {
        "git_spawns": spawns,
        "observations": len(identities),
        "errors": errors,
        **barrier_evidence,
        "pass": spawns == 4
        and len(identities) == 1
        and not errors
        and barrier_evidence["all_admitted_before_release"],
    }


def warm_restarted_service(run):
    run.service, run.control = run.endpoint.restart(
        run.service,
        run.control,
    )
    warm_queries(run)
    before = run.metrics()
    warm_queries(run)
    after = run.metrics()
    run.result["checks"]["initial_warm_git_work"] = run.result["checks"][
        "warm_git_work"
    ]
    run.result["checks"]["warm_git_work"] = {
        "before": before,
        "after": after,
        "equal": before.get("git") == after.get("git")
        and before["queries"]["object_exchanges"]
        == after["queries"]["object_exchanges"],
        "service_pid": run.service.pid,
    }
    run.result["capabilities"] = run.control.execute(request("capabilities", {}))
    run.result["environment"]["advertised_socket_io_workers"] = run.result[
        "capabilities"
    ]["limits"].get("socket_io_workers")
    run.result["configuration_overrides"] = {
        "cache_bytes": run.result["capabilities"]["limits"]["cache_bytes"],
        "cache_entries": run.result["capabilities"]["limits"]["cache_entries"],
        "manifest_cache_bytes": run.plan.manifest["workload"]["cache_bytes"],
        "note": "Actual advertised implementation budget is declared explicitly; the working set must fit this budget.",
    }


def wait_for_admission(run, requests):
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        durable = sum(
            operation_exists(run.control, req["operation_id"]) for req in requests
        )
        if durable == len(requests) and run.sync_marker.exists():
            return True
        time.sleep(0.005)
    return False


def operation_exists(control, operation_id):
    try:
        control.execute(
            request("operation.show", {"target_operation_id": operation_id})
        )
        return True
    except RuntimeError:
        return False


def burst_errors(answers):
    return [row["error"] for row in answers if row["error"]]
