import hashlib
import json
import tarfile


def archive_raw(output):
    files = sorted(output.glob("*.csv"))
    if not files:
        return None
    checksums = {}
    for path in files:
        with open(path, "rb") as stream:
            checksums[path.name] = {
                "bytes": path.stat().st_size,
                "sha256": hashlib.file_digest(stream, "sha256").hexdigest(),
            }
    temporary = output / "raw-samples.tar.gz.tmp"
    destination = output / "raw-samples.tar.gz"
    with tarfile.open(temporary, "w:gz", compresslevel=6) as archive:
        for path in files:
            archive.add(path, arcname=path.name, recursive=False)
    with tarfile.open(temporary, "r:gz") as archive:
        for name, expected in checksums.items():
            with archive.extractfile(name) as stream:
                if (
                    hashlib.file_digest(stream, "sha256").hexdigest()
                    != expected["sha256"]
                ):
                    raise RuntimeError("Archived sample checksum mismatch")
    temporary.replace(destination)
    (output / "raw-checksums.json").write_text(json.dumps(checksums, indent=2))
    for path in files:
        path.unlink()
    with open(destination, "rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    return {
        "path": destination.name,
        "sha256": digest,
        "files": len(files),
        "bytes": destination.stat().st_size,
        "checksum_manifest": "raw-checksums.json",
    }
