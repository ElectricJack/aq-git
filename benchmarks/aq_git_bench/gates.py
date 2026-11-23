def cold_answer_agreement(service, baseline):
    expected = service.get("finite_workload", {}).get("expected_keys", 0)
    actual = service.get("answer_digests", {})
    reference = baseline.get("answer_digests", {})
    mismatches = mismatched_indices(actual, reference)
    return {
        "expected_keys": expected,
        "service_answers": len(actual),
        "baseline_answers": len(reference),
        "mismatched_indices": mismatches,
        "pass": expected > 0
        and matching_answer_count(actual, reference, expected)
        and not mismatches
        and finite_workload_completed(service, baseline),
        "method": "SHA-256 of canonical decoded JSON per input index, computed after latency clock stops",
    }


def mismatched_indices(actual, reference):
    return sorted(
        k for k in actual.keys() | reference.keys() if actual.get(k) != reference.get(k)
    )


def matching_answer_count(actual, reference, expected):
    return len(actual) == len(reference) == expected


def finite_workload_completed(service, baseline):
    return service.get("finite_workload", {}).get("complete", False) and baseline.get(
        "finite_workload", {}
    ).get("complete", False)


def latency_gate(m, targets):
    threshold95 = (
        targets["batch_p95_ms"] if m["scenario"] == "batch" else targets["warm_p95_ms"]
    )
    threshold99 = (
        targets["batch_p99_ms"]
        if m["scenario"] == "batch"
        else targets["mixed_p99_ms"]
        if m["scenario"] == "mixed"
        else targets["warm_p99_ms"]
    )
    return {
        "scenario": m["scenario"],
        "repetition": m["repetition"],
        "clients": m["clients"],
        "latency_pass": m["p95_ms"] is not None
        and m["p95_ms"] <= threshold95
        and (m["p99_ms"] <= threshold99),
        "zero_errors": not m["errors"],
    }


def comparison_measurement(m, result):
    return next(
        (
            p
            for p in result["measurements"]
            if p["backend"] == "git_process"
            and all((p[k] == m[k] for k in ("scenario", "repetition", "clients")))
        ),
        None,
    )


def compare_backends(gate, m, peer):
    if not peer:
        return
    if m["p95_ms"] and peer["queries_per_second"]:
        gate.update(
            p95_speedup=peer["p95_ms"] / m["p95_ms"],
            throughput_speedup=m["queries_per_second"] / peer["queries_per_second"],
        )
    gate["classes"] = {
        operation: {
            "p95_speedup": peer["classes"][operation]["p95_ms"] / values["p95_ms"],
            "throughput_speedup": values["queries_per_second"]
            / peer["classes"][operation]["queries_per_second"],
        }
        for operation, values in m["classes"].items()
        if operation in peer["classes"]
        and values["p95_ms"]
        and peer["classes"][operation]["queries_per_second"]
    }


def check_cold_gate(gate, m, peer):
    gate["p6_classes"] = {
        op: {
            "service_p95_ms": values["p95_ms"],
            "process_git_p95_ms": peer["classes"][op]["p95_ms"],
            "pass": values["p95_ms"] <= peer["classes"][op]["p95_ms"] * 2 + 2,
        }
        for op, values in m["classes"].items()
        if op in peer["classes"]
    }
    gate["cold_answer_agreement"] = cold_answer_agreement(m, peer)
    gate["p6_pass"] = (
        gate["cold_answer_agreement"]["pass"]
        and gate["zero_errors"]
        and finite_workload_completed(m, peer)
        and all((v["pass"] for v in gate["p6_classes"].values()))
    )


def check_mixed_gate(gate, m):
    read_peer = m.get("matched_read_control")
    gate["p5_numerical_diagnostic_pass"] = bool(
        read_peer
        and m["p99_ms"] <= 50
        and (
            m["logical_queries_per_second"]
            >= read_peer["logical_queries_per_second"] * 0.8
        )
        and background_passed(m["background"])
        and (not m["errors"])
    )
    gate["p5_pass"] = gate["p5_numerical_diagnostic_pass"] and m["seconds"] >= 60
    gate["read_throughput_ratio"] = (
        m["logical_queries_per_second"] / read_peer["logical_queries_per_second"]
        if read_peer
        else None
    )


def measurement_gate(m, targets, result):
    gate = latency_gate(m, targets)
    peer = comparison_measurement(m, result)
    compare_backends(gate, m, peer)
    if m["scenario"] == "cold" and peer:
        check_cold_gate(gate, m, peer)
    if m["scenario"] == "mixed":
        check_mixed_gate(gate, m)
    if m["scenario"] == "overload":
        gate["overload_recovered"] = m["recovery"]["restored_within_5_seconds"]
    if m["scenario"] == "churn":
        gate["p7_partial_pass"] = within_churn_bounds(m)
    if m["clients"] == 32 and m["scenario"] == "read":
        check_read_gate(gate, m)
    return gate


def check_read_gate(gate, m):
    gate.update(
        p2_pass=gate.get("p95_speedup", 0) >= 10
        and gate.get("throughput_speedup", 0) >= 5,
        p3_numerical_diagnostic_pass=m["queries_per_second"] >= 1000
        and gate["latency_pass"]
        and gate["zero_errors"],
    )
    gate["p3_pass"] = gate["p3_numerical_diagnostic_pass"] and m["seconds"] >= 60


def measurement_gates(result, targets):
    return [
        measurement_gate(m, targets, result)
        for m in result["measurements"]
        if m["backend"] == "service"
    ]


def acceptance_matrix(result):
    measurements = result["measurements"]
    settings = result["overrides"]
    required = {1, 8, 32}
    rep = settings["repetitions"]
    gates = result.get("gates", [])
    read_gates = scenario_rows(gates, "read")
    batch_gates = scenario_rows(gates, "batch")
    cold_gates = scenario_rows(gates, "cold")
    mixed_gates = scenario_rows(gates, "mixed")
    reads32 = [g for g in read_gates if g["clients"] == 32]
    cold1 = [g for g in cold_gates if g["clients"] == 1]
    return {
        "reference_fixture": result["reference_sized_fixture"],
        "declared_machine": machine_declared(result["environment"]),
        "durations_and_repetitions": full_reference_schedule(settings, required),
        "exact_query_agreement": not result["checks"]["correctness"]["mismatches"],
        "p1": warm_latency_passed(
            measurements, read_gates + batch_gates, rep * len(required)
        ),
        "p2": repeats_pass(reads32, "p2_pass", rep),
        "p3": settings["seconds"] >= 60 and repeats_pass(reads32, "p3_pass", rep),
        "p4_warm": result["checks"]["warm_git_work"]["equal"],
        "p4_cold": result["checks"].get("singleflight", {}).get("pass", False),
        "p4_sync": result["checks"].get("shared_sync", {}).get("pass", False),
        "p5": settings["mixed_seconds"] >= 60
        and rep >= 5
        and repeats_pass(mixed_gates, "p5_pass", rep),
        "p6": repeats_pass(cold1, "p6_pass", rep),
        "p7": boundedness_passed(measurements, rep),
        "agent_loop": workflow_passed(
            result.get("agent_loops", []), rep * len(required) * 3, "errors"
        ),
        "workflow_budgets": workflow_passed(
            result.get("workflow_budgets", []), rep, "error"
        ),
        "all_scenarios_selected": not result["not_run"],
    }


def background_passed(background):
    return (
        background["all_four_classes_progressed"]
        and background["four_way_overlap_seconds"] > 0
        and all(not event["error"] for event in background["operations"])
        and not background["cleanup_errors"]
    )


def scenario_rows(rows, scenario):
    return [m for m in rows if m["scenario"] == scenario]


def repeats_pass(rows, flag, expected):
    return len(rows) >= expected and all(r.get(flag, False) for r in rows)


def warm_latency_passed(measurements, gates, expected):
    service = [m for m in measurements if m["backend"] == "service"]
    enough = all(
        len(scenario_rows(service, scenario)) >= expected
        for scenario in ("read", "batch")
    )
    return enough and all(g["latency_pass"] and g["zero_errors"] for g in gates)


def boundedness_passed(measurements, repetitions):
    churn = scenario_rows(measurements, "churn")
    overload = scenario_rows(measurements, "overload")
    return (
        len(churn) >= repetitions
        and all(churn_passed(m) for m in churn)
        and len(overload) >= repetitions
        and all(m["recovery"]["restored_within_5_seconds"] for m in overload)
    )


def churn_passed(m):
    return m["seconds"] >= 1800 and within_churn_bounds(m)


def within_churn_bounds(m):
    bounded = m.get("boundedness", {})
    return all(
        bounded.get(k, False)
        for k in (
            "evictions_observed",
            "cache_within_advertised_bound",
            "recovered_within_5_seconds",
        )
    )


def workflow_passed(rows, expected, error_field):
    return len(rows) >= expected and all(not r[error_field] for r in rows)


def machine_declared(environment):
    return bool(environment.get("cpu_model") and environment.get("filesystem"))


def full_reference_schedule(settings, required):
    return (
        settings["seconds"] >= 60
        and settings["repetitions"] >= 5
        and required.issubset(settings["clients"])
    )
