import json
import shutil
import socket
import subprocess
import time
import uuid

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


def request(op, params, repo=None, mutation=False):
    return envelope(op, params, (repo, 1), mutation)


def registration(params, repo):
    return envelope("repo.register", params, (repo, 0), True)


def envelope(op, params, binding, mutation):
    repo, generation = binding
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
