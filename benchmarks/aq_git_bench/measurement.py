import csv
import hashlib
import json
import multiprocessing
import os
import pathlib
import queue as thread_queue
import random
import signal
import time
from dataclasses import dataclass

from .protocol import Client, request
from .resources import ResourceMonitor, summarize


def collect_finite_workload(queue, workers, budget_seconds):
    deadline = time.monotonic() + budget_seconds + 1
    results = []
    try:
        for _ in workers:
            results.append(queue.get(timeout=max(0.001, deadline - time.monotonic())))
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
    return results


@dataclass
class Workload:
    kind: str
    queries: list
    paths: dict
    sock: object
    clients: int
    seconds: float
    seed: int
    raw_path: pathlib.Path
    arrival_rate: float
    batch: bool
    unique: bool
    unique_budget_seconds: float


class Measurement:
    def __init__(self, workload, baseline):
        self.workload = workload
        self.baseline = baseline
        self.context = multiprocessing.get_context("fork")
        self.barrier = self.context.Barrier(workload.clients + 1)
        self.start_value = self.context.Value("d", 0.0)
        self.queue = self.context.Queue()

    def run(self, service_pid, background):
        workers = [
            self.context.Process(target=self.worker, args=(i,))
            for i in range(self.workload.clients)
        ]
        for child in workers:
            child.start()
        monitor = ResourceMonitor(self.workload.raw_path, service_pid, background)
        monitor.start()
        cleanup = background() if background else None
        self.start_value.value = time.perf_counter()
        self.barrier.wait()
        if self.workload.unique:
            results = collect_finite_workload(
                self.queue, workers, self.workload.unique_budget_seconds
            )
        else:
            results = [
                self.queue.get(timeout=self.workload.seconds + 350) for _ in workers
            ]
        for child in workers:
            child.join(timeout=5)
        measured_end = time.perf_counter()
        background_result = (
            cleanup(self.start_value.value, measured_end) if cleanup else None
        )
        resource_summary = monitor.finish()
        elapsed = measured_end - self.start_value.value
        summary = self.summary(results, elapsed)
        summary["background"] = background_result
        summary["service_resources"] = resource_summary if service_pid else None
        return summary

    def summary(self, results, elapsed):
        summary = summarize(
            [v for samples, _, _ in results for v in samples],
            elapsed,
            [e for _, errors, _ in results for e in errors][:20],
        )
        summary["error_count"] = sum((count for _, _, count in results))
        if self.workload.unique:
            self.read_digests(summary)
        summary["requests_per_second"] = summary["queries_per_second"]
        summary["logical_queries_per_second"] = summary["queries_per_second"] * (
            100 if self.workload.batch else 1
        )
        summary["logical_queries"] = summary["successful_queries"] * (
            100 if self.workload.batch else 1
        )
        summary["driver"] = "one Python OS process per client"
        return summary

    def read_digests(self, summary):
        digests = {}
        for index in range(self.workload.clients):
            answer_path = pathlib.Path(
                str(self.workload.raw_path) + f".{index}.answers.json"
            )
            if answer_path.exists():
                digests.update(json.loads(answer_path.read_text()))
        summary["answer_digests"] = digests
        summary["finite_workload"] = {
            "expected_keys": len(self.workload.queries),
            "completed_keys": summary["successful_queries"],
            "budget_seconds": self.workload.unique_budget_seconds,
            "complete": summary["successful_queries"] == len(self.workload.queries)
            and summary["error_count"] == 0,
        }

    def worker(self, index):
        worker = ClientWorker(self.workload, index)
        if self.workload.unique:
            os.setsid()
        try:
            executor = (
                Client(self.workload.sock)
                if self.workload.kind == "service"
                else self.baseline(
                    self.workload.paths, self.workload.kind == "git_persistent"
                )
            )
        except Exception as error:
            self.barrier.wait()
            self.queue.put(([], [str(error)], 1))
            return
        self.barrier.wait()
        try:
            worker.sample(executor, self.start_value.value)
        finally:
            executor.close()
        worker.finish(self.queue)


class ClientWorker:
    def __init__(self, workload, index):
        self.workload = workload
        self.index = index
        self.digests = {}
        self.rng = random.Random(workload.seed + index)
        self.samples = []
        self.errors = []
        self.error_count = 0
        self.owned = workload.queries[index :: workload.clients]

    def sample(self, executor, start):
        end = start + (
            self.workload.unique_budget_seconds
            if self.workload.unique
            else self.workload.seconds
        )
        with open(
            str(self.workload.raw_path) + f".{self.index}.csv", "w", newline=""
        ) as stream:
            raw = csv.writer(stream)
            raw.writerow(["operation", "latency_ms", "success", "error"])
            n = 0
            while (
                n < len(self.owned)
                if self.workload.unique
                else time.perf_counter() < end
            ):
                scheduled = (
                    start + n * self.workload.clients / self.workload.arrival_rate
                    if self.workload.arrival_rate
                    else time.perf_counter()
                )
                if self.budget_exhausted(n, scheduled, end):
                    break
                delay = scheduled - time.perf_counter()
                if delay > 0:
                    time.sleep(delay)
                query = (
                    self.owned[n]
                    if self.workload.unique
                    else self.rng.choice(self.workload.queries)
                )
                self.execute_sample(executor, (n, query, scheduled), raw)
                n += 1

    def budget_exhausted(self, n, scheduled, end):
        missed_deadline = time.perf_counter() >= end
        schedule_exhausted = not self.workload.unique and scheduled > end
        if not missed_deadline and not schedule_exhausted:
            return False
        if self.workload.unique and n < len(self.owned):
            self.error_count += 1
            self.errors.append(
                "finite cold workload budget exceeded before all keys completed"
            )
        return True

    def select_batch(self, q):
        repo = q["repository_id"]
        items = [x for x in self.workload.queries if x["repository_id"] == repo]
        chosen = [self.rng.choice(items) for _ in range(100)]
        q = request(
            "query.batch",
            {
                "items": [
                    {"operation": x["operation"], "params": x["params"]} for x in chosen
                ]
            },
            repo,
        )
        return q, chosen

    def prepare_sample(self, query):
        if not self.workload.batch:
            return query["operation"], query, None
        batch, chosen = self.select_batch(query)
        return "batch", batch, chosen

    def query_executor(self, executor, query, chosen):
        if not chosen or self.workload.kind == "service":
            return executor.execute(query)
        if self.workload.unique:
            return {
                "items": [
                    {"ok": True, "result": executor.execute(item)} for item in chosen
                ]
            }
        for item in chosen:
            executor.execute(item)
        return None

    def execute_sample(self, executor, sample, raw):
        n, q, scheduled = sample
        op, q, chosen = self.prepare_sample(q)
        before = scheduled if self.workload.arrival_rate else time.perf_counter()
        try:
            answer = self.query_executor(executor, q, chosen)
            ms = (time.perf_counter() - before) * 1000
            if self.workload.unique:
                self.record_answer(n, answer)
            self.samples.append((op, ms))
            raw.writerow([op, round(ms, 6), 1, ""])
        except Exception as e:
            self.error_count += 1
            if len(self.errors) < 20:
                self.errors.append(str(e))
            raw.writerow([op, (time.perf_counter() - before) * 1000, 0, str(e)])
            time.sleep(0.001)

    def record_answer(self, n, answer):
        # Hash decoded answers after stopping the latency clock.
        self.digests[str(self.index + n * self.workload.clients)] = hashlib.sha256(
            json.dumps(
                answer,
                sort_keys=True,
                separators=(",", ":"),
                ensure_ascii=True,
            ).encode()
        ).hexdigest()

    def finish(self, output_queue):
        if self.workload.unique:
            pathlib.Path(
                str(self.workload.raw_path) + f".{self.index}.answers.json"
            ).write_text(json.dumps(self.digests, sort_keys=True))
        output_queue.put((self.samples, self.errors, self.error_count))
