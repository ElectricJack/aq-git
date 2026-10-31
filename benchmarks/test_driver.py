"""Transport regressions for the benchmark control connection (standard library only)."""

import importlib.util
import json
import pathlib
import socket
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "benchmark_driver", pathlib.Path(__file__).with_name("run.py")
)
driver = importlib.util.module_from_spec(spec)
spec.loader.exec_module(driver)


class ControlConnectionTests(unittest.TestCase):
    def exchange(self, handler, action):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "socket"
            listener = socket.socket(socket.AF_UNIX)
            listener.bind(str(path))
            listener.listen()
            listener.settimeout(3)
            errors = []

            def serve():
                try:
                    handler(listener)
                except BaseException as error:
                    errors.append(error)
                finally:
                    listener.close()

            thread = threading.Thread(target=serve, daemon=True)
            thread.start()
            client = driver.Client(path)
            try:
                action(client)
            finally:
                client.close()
                thread.join(timeout=4)
            self.assertFalse(thread.is_alive(), "test server did not finish")
            self.assertEqual(errors, [])

    def test_lost_ack_retries_identical_mutation(self):
        received = []

        def serve(listener):
            for attempt in range(2):
                with listener.accept()[0] as connection:
                    connection.settimeout(3)
                    with connection.makefile("rb") as reader:
                        received.append(json.loads(reader.readline()))
                    if attempt:
                        connection.sendall(
                            json.dumps(
                                {
                                    "request_id": received[-1]["request_id"],
                                    "ok": True,
                                    "result": {"accepted": True},
                                }
                            ).encode()
                            + b"\n"
                        )

        request = driver.request(
            "repo.sync",
            {"refs": ["refs/heads/main"]},
            "1524d5ba-1656-5b31-80a7-e340b29bb67d",
            True,
        )
        self.exchange(
            serve,
            lambda client: self.assertEqual(
                client.execute(request), {"accepted": True}
            ),
        )
        self.assertEqual(received, [request, request])
        self.assertEqual(len({entry["operation_id"] for entry in received}), 1)

    def test_idle_control_connection_is_reopened_before_mutation(self):
        received = []

        def serve(listener):
            with listener.accept()[0] as idle:
                idle.settimeout(3)
                self.assertEqual(idle.recv(1), b"")
            with listener.accept()[0] as connection:
                with connection.makefile("rb") as reader:
                    request = json.loads(reader.readline())
                received.append(request)
                connection.sendall(
                    json.dumps(
                        {"request_id": request["request_id"], "ok": True, "result": {}}
                    ).encode()
                    + b"\n"
                )

        request = driver.request(
            "repo.sync", {}, "1524d5ba-1656-5b31-80a7-e340b29bb67d", True
        )

        def execute(client):
            client.last_activity = time.monotonic() - 26
            self.assertEqual(client.execute(request), {})
            self.assertEqual(client.reconnects, 1)

        self.exchange(serve, execute)
        self.assertEqual(received, [request])

    def test_domain_overload_is_reported_without_retry(self):
        def serve(listener):
            with listener.accept()[0] as connection:
                with connection.makefile("rb") as reader:
                    request = json.loads(reader.readline())
                connection.sendall(
                    json.dumps(
                        {
                            "request_id": request["request_id"],
                            "ok": False,
                            "error": {"code": "overloaded"},
                        }
                    ).encode()
                    + b"\n"
                )

        def execute(client):
            with self.assertRaisesRegex(RuntimeError, "overloaded"):
                client.execute(driver.request("capabilities", {}))
            self.assertEqual(client.reconnects, 0)

        self.exchange(serve, execute)


class SlowFiniteBaseline:
    def __init__(self, *_):
        pass

    def execute(self, _):
        time.sleep(0.02)

    def close(self):
        pass


class FiniteColdTests(unittest.TestCase):
    def measure(self, budget=2):
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(driver, "GitBaseline", SlowFiniteBaseline):
                return driver.measure(
                    "git_process",
                    [{"operation": "inspect"}] * 4,
                    {},
                    None,
                    1,
                    0.001,
                    1,
                    pathlib.Path(directory) / "raw",
                    unique=True,
                    unique_budget_seconds=budget,
                )

    def test_cold_drains_entire_input_despite_short_timed_duration(self):
        result = self.measure()
        self.assertEqual(result["successful_queries"], 4)
        self.assertTrue(result["finite_workload"]["complete"])
        self.assertEqual(len(result["answer_digests"]), 4)
        self.assertGreater(result["seconds"], 0.001)

    def test_cold_answer_comparison_rejects_changed_or_missing_answer(self):
        result = self.measure()
        self.assertTrue(driver.cold_answer_agreement(result, result)["pass"])
        changed = dict(result, answer_digests=dict(result["answer_digests"]))
        changed["answer_digests"]["1"] = "different canonical decoded answer"
        self.assertFalse(driver.cold_answer_agreement(result, changed)["pass"])
        del changed["answer_digests"]["1"]
        self.assertFalse(driver.cold_answer_agreement(result, changed)["pass"])

    def test_cold_budget_exhaustion_is_explicit_failure(self):
        result = self.measure(budget=0.03)
        self.assertFalse(result["finite_workload"]["complete"])
        self.assertGreater(result["error_count"], 0)
        self.assertLess(result["successful_queries"], 4)


class ReportQualificationTests(unittest.TestCase):
    def result(self, seconds=5, repetitions=1):
        return {
            "measurements": [],
            "gates": [
                {
                    "scenario": "read",
                    "clients": 32,
                    "p3_pass": True,
                    "p3_numerical_diagnostic_pass": True,
                },
                {
                    "scenario": "mixed",
                    "clients": 32,
                    "p5_pass": True,
                    "p5_numerical_diagnostic_pass": True,
                },
            ],
            "overrides": {
                "repetitions": repetitions,
                "seconds": seconds,
                "mixed_seconds": seconds,
                "clients": [32],
            },
            "reference_sized_fixture": True,
            "environment": {},
            "checks": {
                "correctness": {"mismatches": []},
                "warm_git_work": {"equal": True},
            },
            "not_run": [],
        }

    def test_short_numerical_success_cannot_qualify_p3_or_p5(self):
        result = self.result()
        matrix = driver.acceptance_matrix(result)
        self.assertFalse(matrix["p3"])
        self.assertFalse(matrix["p5"])
        self.assertTrue(result["gates"][0]["p3_numerical_diagnostic_pass"])
        self.assertTrue(result["gates"][1]["p5_numerical_diagnostic_pass"])

    def test_real_sixty_seconds_qualifies_p3_but_not_single_repeat_p5(self):
        matrix = driver.acceptance_matrix(self.result(seconds=60))
        self.assertTrue(matrix["p3"])
        self.assertFalse(matrix["p5"])


if __name__ == "__main__":
    unittest.main()
