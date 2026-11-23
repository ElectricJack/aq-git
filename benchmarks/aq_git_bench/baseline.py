import subprocess

from .protocol import ENV, GIT, byte_path, git, oid


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
        if len(header) != 3 or (header[0].decode(), header[1]) != (h, b"commit"):
            raise RuntimeError("invalid baseline batch frame")
        data = p.stdout.read(int(header[2]))
        assert p.stdout.read(1) == b"\n"
        return data

    def execute(self, q):
        path = self.paths[q["repository_id"]]
        p = q["params"]
        op = q["operation"]
        if op == "inspect":
            return self.inspect(path, p)
        if op == "contains":
            return self.contains(path, p)
        if op == "tree":
            entries = self.tree(path, p)
        elif op == "diff":
            entries = self.diff(path, p)
        else:
            raise RuntimeError("unrecognized baseline query")
        limit = p.get("limit", 100)
        return {"entries": entries[:limit], "truncated": len(entries) > limit}

    def inspect(self, path, p):
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

    @staticmethod
    def contains(path, p):
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

    @staticmethod
    def tree(path, p):
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
        return entries

    @staticmethod
    def diff(path, p):
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
        return entries

    def close(self):
        for p in self.readers.values():
            p.stdin.close()
            try:
                p.wait(timeout=2)
            except subprocess.TimeoutExpired:
                p.kill()
                p.wait()
