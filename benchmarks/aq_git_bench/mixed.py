import os
import queue as thread_queue
import shutil
import signal
import subprocess
import threading
import time
import uuid

from .protocol import Client, oid, registration, request
from .resources import process_tree
from .service import ServerLocation, candidate_fixture, release, retain


class MixedBackground:
    def __init__(self, run):
        self.run = run
        repos = list(self.run.paths)
        self.repo_a, self.repo_b = repos[:2]
        self.head_a = self.run.control.execute(
            request("repo.sync", {}, self.repo_a, True)
        )["refs"]["refs/heads/main"]
        self.head_b = self.run.control.execute(
            request("repo.sync", {}, self.repo_b, True)
        )["refs"]["refs/heads/main"]
        self.handle_a = retain(self.run.control, self.repo_a, self.head_a)
        self.register_candidates()
        self.register_worker()
        self.register_stalled_fetch()
        self.additional_pids = [self.worker.pid]

    def register_candidates(self):
        candidate_remote, candidate_refs = candidate_fixture(
            self.run.paths[self.repo_b], self.head_b, self.run.work
        )
        self.candidate_repo = str(uuid.uuid4())
        self.run.control.execute(
            registration(
                {
                    "identity": "benchmark-candidate-" + self.candidate_repo,
                    "mode": "central",
                    "remote_url": str(candidate_remote),
                    "allow_file_transport": True,
                    "allowed_refs": ["refs/heads/main", *candidate_refs],
                },
                self.candidate_repo,
            )
        )
        candidate_observation = self.run.control.execute(
            request("repo.sync", {}, self.candidate_repo, True)
        )
        self.candidate_sources = [
            {
                "oid": oid(candidate_observation["refs"][ref]),
                "retention_handle": retain(
                    self.run.control,
                    self.candidate_repo,
                    candidate_observation["refs"][ref],
                ),
            }
            for ref in candidate_refs
        ]

    def register_worker(self):
        self.worker_state = self.run.work / f"mixed-worker-{uuid.uuid4()}"
        self.worker_socket = self.run.work / f"mixed-{os.getpid()}.sock"
        self.worker = ServerLocation(
            self.run.plan.args.binary,
            self.worker_state,
            self.worker_socket,
            self.run.log,
        ).start()
        wc = Client(self.worker_socket)
        wc.execute(
            registration(
                {"identity": "benchmark-" + self.repo_a, "mode": "worker"}, self.repo_a
            )
        )
        wc.close()

    def register_stalled_fetch(self):
        self.stall_repo = str(uuid.uuid4())
        self.run.control.execute(
            registration(
                {
                    "identity": "benchmark-stall",
                    "mode": "central",
                    "remote_url": str(self.run.stall_remote),
                    "allow_file_transport": True,
                    "allowed_refs": ["refs/heads/main"],
                },
                self.stall_repo,
            )
        )

    def __call__(self):
        self.stop = threading.Event()
        self.handoffs = thread_queue.Queue(maxsize=2)
        self.events = []
        self.events_lock = threading.Lock()
        self.began = time.perf_counter()
        self.threads = [
            threading.Thread(target=f)
            for f in (self.exporter, self.importer, self.candidate, self.stalled)
        ]
        for thread in self.threads:
            thread.start()
        deadline = time.monotonic() + 10
        while not self.run.stall_marker.exists() and time.monotonic() < deadline:
            time.sleep(0.01)
        return self.cleanup

    def record(self, kind, start, outcome):
        error, completed = outcome
        with self.events_lock:
            self.events.append(
                {
                    "kind": kind,
                    "started_seconds": start - self.began,
                    "completed_seconds": (
                        completed if completed is not None else time.perf_counter()
                    )
                    - self.began,
                    "error": error,
                }
            )

    def exporter(self):
        client = Client(self.run.sock)
        try:
            while not self.stop.is_set():
                start = time.perf_counter()
                try:
                    out = client.execute(
                        request(
                            "artifact.export",
                            {
                                "source": oid(self.head_a),
                                "base": oid(self.head_a),
                                "retention_handle": self.handle_a,
                            },
                            self.repo_a,
                            True,
                        )
                    )
                    completed = time.perf_counter()
                    handle = out["artifact_handle"]
                    incoming = self.worker_state / "artifacts/incoming" / handle
                    incoming.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copytree(self.run.state / "artifacts" / handle, incoming)
                    self.hand_off(handle)
                    self.record("export", start, (None, completed))
                except Exception as e:
                    self.record("export", start, (str(e), None))
                    break
        finally:
            client.close()

    def importer(self):
        client = Client(self.worker_socket)
        try:
            while not self.stop.is_set() or not self.handoffs.empty():
                try:
                    handle = self.handoffs.get(timeout=0.05)
                except thread_queue.Empty:
                    continue
                start = time.perf_counter()
                try:
                    client.execute(
                        request(
                            "artifact.import",
                            {"artifact_handle": handle},
                            self.repo_a,
                            True,
                        )
                    )
                    completed = time.perf_counter()
                    release(client, self.repo_a, handle)
                    central = Client(self.run.sock)
                    release(central, self.repo_a, handle)
                    central.close()
                    self.record("import", start, (None, completed))
                except Exception as e:
                    self.record("import", start, (str(e), None))
                    break
        finally:
            client.close()

    def candidate(self):
        client = Client(self.run.sock)
        try:
            while not self.stop.is_set():
                start = time.perf_counter()
                try:
                    out = client.execute(
                        request(
                            "candidate.create",
                            {
                                "base": oid(self.head_b),
                                "sources": self.candidate_sources,
                                "commit": {
                                    "name": "Benchmark",
                                    "email": "bench@example.test",
                                    "message": "mixed candidate",
                                },
                            },
                            self.candidate_repo,
                            True,
                        )
                    )
                    completed = time.perf_counter()
                    release(client, self.candidate_repo, out["retention_handle"])
                    self.record("candidate", start, (None, completed))
                except Exception as e:
                    self.record("candidate", start, (str(e), None))
                    break
        finally:
            client.close()

    def stalled(self):
        client = Client(self.run.sock)
        start = time.perf_counter()
        try:
            client.execute(request("repo.sync", {}, self.stall_repo, True))
            self.record("stalled_fetch", start, (None, None))
        except Exception as e:
            self.record("stalled_fetch", start, (str(e), None))
        finally:
            client.close()

    def hand_off(self, handle):
        while not self.stop.is_set():
            try:
                self.handoffs.put(handle, timeout=0.05)
                break
            except thread_queue.Full:
                pass

    def cleanup(self, measured_start, measured_end):
        self.stop.set()
        self.run.stall_release.touch()
        for thread in self.threads:
            thread.join(timeout=330)
        cleanup_errors = self.release_resources()
        worker_tree = process_tree(self.worker.pid)
        self.worker.send_signal(signal.SIGTERM)
        try:
            self.worker.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.worker.kill()
            self.worker.wait()
        lo = measured_start - self.began
        hi = measured_end - self.began
        overlaps, common_seconds = self.overlap(lo, hi)
        return {
            "operations": self.events,
            "measurement_window_seconds": {"start": lo, "end": hi},
            "measured_overlap_seconds_by_kind": overlaps,
            "four_way_overlap_seconds": common_seconds,
            "four_way_overlap_fraction": common_seconds / (hi - lo),
            "candidate_workload": {
                "sources": len(self.candidate_sources),
                "unique_sources": len(
                    {source["oid"]["hex"] for source in self.candidate_sources}
                ),
                "shape": "independent commits from one base, each adding a distinct file; 63 real clean merge steps",
            },
            "cleanup_errors": cleanup_errors,
            "stalled_fetch_entered": self.run.stall_marker.exists(),
            "worker_process_tree": worker_tree,
            "all_four_classes_progressed": all(
                (
                    any((e["kind"] == kind and (not e["error"]) for e in self.events))
                    for kind in ("export", "import", "candidate", "stalled_fetch")
                )
            ),
            "note": "Export/import pipeline is bounded to two queued bundles; retained-resource and filesystem usage is reported, not silently deleted.",
        }

    def release_resources(self):
        cleanup_errors = []
        cleanup_client = Client(self.run.sock)
        for resource_repo, handle in [
            (self.repo_a, self.handle_a),
            *[
                (self.candidate_repo, source["retention_handle"])
                for source in self.candidate_sources
            ],
        ]:
            try:
                release(cleanup_client, resource_repo, handle)
            except Exception as error:
                cleanup_errors.append(str(error))
        cleanup_client.close()
        return cleanup_errors

    def overlap(self, lo, hi):
        common = [(lo, hi)]
        overlaps = {}
        for kind in ("export", "import", "candidate", "stalled_fetch"):
            intervals = self.completed_intervals(kind, lo, hi)
            overlaps[kind] = sum(end - start for start, end in intervals)
            common = intersect_intervals(common, intervals)
        common_seconds = sum(end - start for start, end in common)
        return overlaps, common_seconds

    def completed_intervals(self, kind, lo, hi):
        return [
            (
                max(lo, event["started_seconds"]),
                min(hi, event["completed_seconds"]),
            )
            for event in self.events
            if event["kind"] == kind
            and not event["error"]
            and event["started_seconds"] < hi
            and event["completed_seconds"] > lo
        ]


def intersect_intervals(common, intervals):
    return [
        (max(start, other_start), min(end, other_end))
        for start, end in common
        for other_start, other_end in intervals
        if max(start, other_start) < min(end, other_end)
    ]
