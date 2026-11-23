import json
import multiprocessing
import time

from .baseline import GitBaseline
from .protocol import Client, git, oid, request
from .resources import percentile


class AgentLoop:
    def __init__(self, run, backend, clients):
        self.run = run
        self.clients = clients
        self.backend = backend
        self.context = multiprocessing.get_context("fork")
        self.inboxes = [self.context.Queue() for _ in range(self.clients)]
        self.outbox = self.context.Queue()

    def worker(self, index):
        executor = (
            Client(self.run.sock)
            if self.backend == "service"
            else GitBaseline(self.run.paths, self.backend == "git_persistent")
        )
        try:
            while True:
                payload = self.inboxes[index].get()
                if payload is None:
                    break
                self.measure_batch(index, executor, payload)
        finally:
            executor.close()

    def measure_batch(self, index, executor, payload):
        round_id, queries = payload
        start = time.perf_counter()
        try:
            values = self.execute_queries(executor, queries)
            error = None
        except Exception as e:
            values = None
            error = str(e)
        elapsed = (time.perf_counter() - start) * 1000
        self.outbox.put((round_id, index, elapsed, values, error))

    def execute_queries(self, executor, queries):
        if self.backend == "service":
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
        return values

    def measure(self, output):
        self.workers = [
            self.context.Process(target=self.worker, args=(i,))
            for i in range(self.clients)
        ]
        for child in self.workers:
            child.start()
        start = time.perf_counter()
        self.rounds = []
        self.repo = next(iter(self.run.paths))
        self.path = self.run.paths[self.repo]
        previous = git(self.path, ["rev-parse", "refs/heads/main"]).decode().strip()
        original_head = previous
        self.checker = GitBaseline(self.run.paths, True)
        try:
            while time.perf_counter() - start < self.run.plan.seconds:
                previous = self.run_round(previous)
        finally:
            self.checker.close()
            git(self.path, ["update-ref", "refs/heads/main", original_head])
            for inbox in self.inboxes:
                inbox.put(None)
            for child in self.workers:
                child.join(timeout=5)
        return self.report(output)

    def run_round(self, previous):
        sync_start = time.perf_counter()
        observation = self.run.control.execute(
            request("repo.sync", {}, self.repo, True)
        )
        sync_ms = (time.perf_counter() - sync_start) * 1000
        frozen = observation["refs"]["refs/heads/main"]
        new = self.advance(frozen)
        mix = self.queries(previous, frozen)
        expected = [self.checker.execute(q) for q in mix]
        round_id = len(self.rounds)
        decision_start = time.perf_counter()
        for inbox in self.inboxes:
            inbox.put((round_id, mix))
        answers = [self.outbox.get(timeout=350) for _ in self.workers]
        decision_ms = (time.perf_counter() - decision_start) * 1000
        errors = [
            a[4] or "frozen result mismatch"
            for a in answers
            if a[4] or a[3] != expected
        ]
        self.rounds.append(
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
        return frozen

    def advance(self, frozen):
        tree = git(self.path, ["rev-parse", frozen + "^{tree}"]).decode().strip()
        new = (
            git(
                self.path,
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
        git(self.path, ["update-ref", "refs/heads/main", new, frozen])
        return new

    def queries(self, previous, frozen):
        return (
            [request("inspect", {"oid": oid(frozen)}, self.repo)] * 40
            + [
                request(
                    "contains",
                    {"ancestor": oid(previous), "descendant": oid(frozen)},
                    self.repo,
                )
            ]
            * 40
            + [request("tree", {"oid": oid(frozen), "limit": 100}, self.repo)] * 10
            + [
                request(
                    "diff",
                    {"base": oid(previous), "target": oid(frozen), "limit": 100},
                    self.repo,
                )
            ]
            * 10
        )

    def report(self, output):
        (output / f"agent-loop-{self.clients}-{self.backend}.json").write_text(
            json.dumps(self.rounds, indent=2)
        )
        return {
            "scenario": "agent-loop",
            "backend": self.backend,
            "clients": self.clients,
            "rounds": len(self.rounds),
            "queries_per_round": 100 * self.clients,
            "errors": [e for r in self.rounds for e in r["errors"]],
            "decision_p95_ms": percentile(
                [r["decision_ms"] for r in self.rounds], 0.95
            ),
            "sync_p95_ms": percentile([r["sync_ms"] for r in self.rounds], 0.95),
            "note": "All backends share the same explicit service sync contract; local read comparisons are separate from refresh timing. Remote advances occur strictly after each frozen observation.",
        }
