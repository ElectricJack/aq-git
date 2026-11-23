import signal
import subprocess
import time
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import BinaryIO

from .protocol import Client, git, oid, registration, request

SERVERS = []


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


@dataclass
class ServerLocation:
    binary: Path
    state: Path
    sock: Path
    log: BinaryIO

    def start(self, git_executable=None):
        command = [
            str(self.binary.resolve()),
            "--state-dir",
            str(self.state),
            "--listen",
            str(self.sock),
        ]
        if git_executable:
            command += ["--git-executable", str(git_executable)]
        process = subprocess.Popen(
            command + ["serve"], stdout=self.log, stderr=self.log
        )
        SERVERS.append(process)
        for _ in range(1000):
            if process.poll() is not None:
                raise RuntimeError("service exited before opening socket")
            if self.sock.exists():
                return process
            time.sleep(0.01)
        raise RuntimeError("service socket startup deadline")

    def restart(self, process, control, git_executable=None):
        control.close()
        process.send_signal(signal.SIGTERM)
        try:
            process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            raise RuntimeError(
                "Service did not shut down cleanly for benchmark restart"
            )
        self.sock.unlink(missing_ok=True)
        process = self.start(git_executable)
        control = Client(self.sock)
        control.execute(
            registration(
                {"identity": "benchmark-private-probe", "mode": "worker"},
                str(uuid.uuid4()),
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
