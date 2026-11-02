#!/usr/bin/env python3
"""Record an unpublished clean-source build; never create a tag or release."""
import gzip
import hashlib
import io
import json
from pathlib import Path
import re
import subprocess
import tarfile
import tomllib

root = Path(__file__).resolve().parent
source = root / 'source'
assets = root / 'assets'
revision = 'e75f7b4bc36a83ae5001fefa0d5f87618e87ad4a'
image = 'rust:1.96.1-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663'
binary = assets / 'aq-git-v0.2.0-linux-x86_64'
archive = assets / 'aq-git-v0.2.0-source.tar.gz'
record_path = assets / 'aq-git-v0.2.0-build-record.json'

def run(args):
    return subprocess.check_output(args, text=True).strip()

def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

assert run(['git', '-C', str(source), 'rev-parse', 'HEAD']) == revision
assert not run(['git', '-C', str(source), 'status', '--porcelain'])
assert run([str(binary), '--version']) == 'aq-git 0.2.0'
manifest = json.loads((root / 'development-manifest.json').read_text())
assert manifest['source']['state'] == 'clean'
assert manifest['source']['head_oid'] == revision
assert manifest['binary']['sha256'] == digest(binary)
assert manifest['workspace_version'] == '0.2.0'
entries = manifest['source']['files']
canonical = json.dumps(entries, sort_keys=True, ensure_ascii=True, separators=(',', ':')).encode('ascii')
assert hashlib.sha256(canonical).hexdigest() == manifest['source']['inventory_sha256']
with tarfile.open(archive, 'r:gz') as tar:
    actual = {}
    for member in tar.getmembers():
        if member.isdir():
            continue
        name = member.name.removeprefix('aq-git-v0.2.0/')
        assert member.name == 'aq-git-v0.2.0/' + name
        content = member.linkname.encode() if member.issym() else tar.extractfile(member).read()
        actual[name] = hashlib.sha256(content).hexdigest()
    expected = {item['path_utf8']: item['sha256'] for item in entries}
    assert actual == expected, 'Archive contents must match the independently inventoried tracked source'
tar_bytes = gzip.decompress(archive.read_bytes())
archive_revision = subprocess.check_output(['git', 'get-tar-commit-id'], input=tar_bytes).decode().strip()
assert archive_revision == revision
elf = run(['readelf', '-W', '--version-info', str(binary)])
(root / 'elf-versions.log').write_text(elf + '\n')
versions = {tuple(map(int, version.split('.'))) for version in re.findall(r'GLIBC_([0-9.]+)', elf)}
maximum_glibc = max(versions)
assert maximum_glibc <= (2, 34)
lock = tomllib.loads((source / 'Cargo.lock').read_text())
qualification = []
for version, cargo_log, check_log in [
    ('2.43.0', 'merged-git-2.43-cargo-test.log', 'merged-git-2.43-check-all.log'),
    ('2.56.0', 'merged-git-2.56-cargo-test.log', 'merged-git-2.56-check-all.log'),
]:
    checks = []
    for command, filename in [('cargo test --locked --workspace', cargo_log), ('scripts/check-all', check_log)]:
        path = source / 'docs/implementation/A3a-checks' / filename
        text = path.read_text()
        count = sum(map(int, re.findall(r'test result: ok\. (\d+) passed;', text)))
        assert count == 121 and 'test result: FAILED' not in text
        checks.append({'command': command, 'passed': count, 'failed': 0, 'log_sha256': digest(path), 'source_log': str(path.relative_to(source))})
    qualification.append({'git_version': version, 'runtime_baseline': 'f5af0b805db8270ccac9efb48787855c5217bc99', 'test_threads': 1, 'qualification_image': 'host Git 2.43 uses extracted Ubuntu sshd; Git 2.56 test-only image sha256:a244094e2b3118966392fc56feb11b24547646d0c3120579b285006686b881b9', 'checks': checks})
record = {
    'build_record_version': 1,
    'package': 'aq-git',
    'version': '0.2.0',
    'artifact_status': 'prepared_unpublished_pending_owner',
    'source_revision': revision,
    'source_selection': 'Merged origin/main D0 f5af0b8 with version bump and fresh two-Git qualification; selected by supervisor msg-f27ff278de7a473da059702b7a23e7a5.',
    'intended_tag': 'v0.2.0',
    'publication_at_build_time': 'No tag or release created; main integration and publication pending owner.',
    'intended_release_url': 'https://github.com/ElectricJack/aq-git/releases/tag/v0.2.0',
    'target_triple': 'x86_64-unknown-linux-gnu',
    'toolchain': {
        'rustc': 'rustc 1.96.1 (31fca3adb 2026-06-26)',
        'rustc_verbose': (root / 'build.log').read_text().split('cargo 1.96.1')[0].strip(),
        'cargo': 'cargo 1.96.1 (356927216 2026-06-26)',
    },
    'builder': {
        'reference': image,
        'digest': image.split('@')[1],
        'image_id': run(['docker', 'image', 'inspect', image, '--format', '{{.Id}}']),
    },
    'build_provenance': {
        'kind': 'observed_locked_build_from_clean_detached_checkout_in_pinned_container',
        'command': ['cargo', 'build', '--release', '--locked', '--target', 'x86_64-unknown-linux-gnu'],
        'source_read_only': True,
        'cargo_build_jobs': 2,
        'network': 'Cargo offline registry cache; no host credentials mounted',
        'build_log_sha256': digest(root / 'build.log'),
    },
    'cargo_lock_sha256': digest(source / 'Cargo.lock'),
    'dependency_inventory': lock['package'],
    'source_inventory_sha256': manifest['source']['inventory_sha256'],
    'archive': {'name': archive.name, 'sha256': digest(archive), 'size_bytes': archive.stat().st_size, 'format': 'git archive --format=tar --prefix=aq-git-v0.2.0/ SOURCE_REVISION | gzip -n', 'embedded_commit_verified': archive_revision},
    'binary': {'name': binary.name, 'sha256': digest(binary), 'size_bytes': binary.stat().st_size, 'format': 'ELF 64-bit x86-64', 'maximum_required_glibc': '.'.join(map(str, maximum_glibc))},
    'archive_sha256': digest(archive),
    'binary_sha256': digest(binary),
    'qualification': qualification,
    'upstream_git': {'version': '2.56.0', 'archive_url': 'https://www.kernel.org/pub/software/scm/git/git-2.56.0.tar.xz', 'archive_sha256': '26c56c296b38c0695b26fa95f475f1d01704d2d38e73465ca30b0b2f5dc789d3', 'build_flags': ['NO_GETTEXT=YesPlease', 'NO_TCLTK=YesPlease']},
    'development_manifest': manifest,
    'remaining_limits': ['P1-P7 performance not certified', 'full compatibility and persisted-boundary crash matrix incomplete', 'host power loss not certified', 'live hosted transport credentials/policy not certified', 'deployed AQ integration not certified', 'upgrade and rollback rehearsal begins with second compatible release', 'main integration, tag, release and remote download verification pending owner'],
}
record_path.write_text(json.dumps(record, indent=2, sort_keys=True) + '\n')
for path in [archive, binary, record_path]:
    path.with_name(path.name + '.sha256').write_text(f'{digest(path)}  {path.name}\n')
print(json.dumps({'source_revision': revision, 'archive_sha256': digest(archive), 'binary_sha256': digest(binary), 'build_record_sha256': digest(record_path), 'max_glibc': record['binary']['maximum_required_glibc'], 'source_files_verified': len(entries)}, indent=2))
