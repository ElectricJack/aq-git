#!/usr/bin/env python3
"""Enforce the ratcheted build budgets in build-budget.toml.

Two numbers decide how slow this workspace is to build and test, and both only
ever grew until they were measured (see AGENTS.md "Build health"):

* integration test binaries: every `tests/*.rs` file (or `[[test]]` target) is
  a separate executable that links the whole library and its dependencies;
* library size: the lines of each crate's lib, which rustc compiles as one unit.

The check fails when either number rises above the value recorded in
build-budget.toml. A recorded value is a ratchet: lower it when the code
shrinks (`--update` does that, and only ever lowers), and raise it only with
an explicit, reviewed edit of build-budget.toml that says why.

    python3 scripts/build-budget.py           # check (CI lint job)
    python3 scripts/build-budget.py --update  # lower recorded values to current
"""

import argparse
import json
import os
import re
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BUDGET = ROOT / "build-budget.toml"


def metadata():
    out = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1", "--offline"],
        cwd=ROOT, check=True, capture_output=True, text=True,
    ).stdout
    return json.loads(out)


def lib_lines(package):
    """Lines of .rs source under the lib target's directory, excluding other targets."""
    targets = package["targets"]
    lib = next((t for t in targets if {"lib", "proc-macro"}.intersection(t["kind"])), None)
    if lib is None:
        return None
    src = Path(lib["src_path"]).parent
    others = {Path(t["src_path"]) for t in targets if t is not lib}
    bin_dir = src / "bin"
    sources = (path for path in src.rglob("*.rs") if path not in others and bin_dir not in path.parents)
    return sum(source_lines(path) for path in sources)


def source_lines(path):
    with path.open("rb") as source:
        return sum(1 for _ in source)


def measure():
    meta = metadata()
    members = set(meta["workspace_members"])
    packages = [p for p in meta["packages"] if p["id"] in members]
    test_binaries = sum(1 for p in packages for t in p["targets"] if "test" in t["kind"])
    lines = {}
    for package in packages:
        count = lib_lines(package)
        if count is not None:
            lines[package["name"]] = count
    return test_binaries, lines


def update(text, test_binaries, ratchets):
    """Rewrite only the numbers that went down; keep comments and layout."""
    def lower(match, current):
        recorded = int(match.group(2))
        return match.group(1) + str(min(recorded, current))

    text = re.sub(r"(?m)^(max\s*=\s*)(\d+)", lambda m: lower(m, test_binaries), text, count=1)
    for name, current in ratchets.items():
        text = re.sub(rf'(?m)^("?{re.escape(name)}"?\s*=\s*)(\d+)', lambda m, c=current: lower(m, c), text)
    return text


def library_over_budget(name, count, ratchet, target):
    if ratchet is None:
        return (f"{name} lib is {count} lines, above the {target}-line target. "
                "Split it into smaller crates.")
    return (f"{name} lib is {count} lines, above its ratchet of {ratchet} (target "
            f"{target}). Move code out of the crate (or delete some) rather than "
            "raising the ratchet.")


def check_libraries(lines, line_target, ratchets):
    errors, notes = [], []
    for name, count in sorted(lines.items(), key=lambda item: -item[1]):
        limit = ratchets.get(name, line_target)
        if name in ratchets or count > line_target * 3 // 4:
            print(f"build-budget: {name} lib {count} lines (limit {limit}, target {line_target})")
        if count > limit:
            errors.append(library_over_budget(name, count, ratchets.get(name), line_target))
        elif name in ratchets and count < limit:
            notes.append(f"{name} lib shrank to {count} lines; lower its ratchet from {limit} "
                         "(scripts/build-budget.py --update).")
    for name in sorted(set(ratchets) - set(lines)):
        notes.append(f"ratchet for {name} names no workspace lib; remove it.")
    return errors, notes


def check_test_binaries(test_binaries, test_budget):
    errors = []
    print(f"build-budget: integration test binaries {test_binaries} (budget {test_budget})")
    if test_binaries > test_budget:
        errors.append(
            f"{test_binaries} integration test binaries exceed the budget of {test_budget}. "
            "Add new tests to an existing test binary (a module of an existing tests/*.rs "
            "file) instead of a new tests/*.rs file.")
    elif test_binaries < test_budget:
        errors.append(
            f"only {test_binaries} integration test binaries remain; lower test_binaries.max "
            f"from {test_budget} to {test_binaries} in {BUDGET.name} (or run "
            "scripts/build-budget.py --update) so the ratchet keeps the gain.")
    return errors


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--update", action="store_true", help="lower recorded ratchets to the current values")
    return parser.parse_args()


def main():
    args = parse_args()
    budget = tomllib.loads(BUDGET.read_text())
    test_budget = budget["test_binaries"]["max"]
    line_target = budget["lib_lines"]["target"]
    ratchets = budget["lib_lines"].get("ratchet", {})
    test_binaries, lines = measure()

    if args.update:
        current = {name: lines.get(name, 0) for name in ratchets}
        BUDGET.write_text(update(BUDGET.read_text(), test_binaries, current))
        print(f"build-budget: recorded values lowered where possible ({BUDGET.name})")
        return 0

    errors = check_test_binaries(test_binaries, test_budget)
    lib_errors, notes = check_libraries(lines, line_target, ratchets)
    errors.extend(lib_errors)
    return report(errors, notes)


def report(errors, notes):
    for note in notes:
        print(f"build-budget: note: {note}")
    for error in errors:
        print(f"::error::build-budget: {error}" if os.environ.get("GITHUB_ACTIONS")
              else f"build-budget: error: {error}", file=sys.stderr)
    if errors:
        return 1
    print("build-budget: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
