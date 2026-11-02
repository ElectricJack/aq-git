"""Regressions for scripts/check-docs (standard library only).

Each test copies the checker into an isolated temporary repository so results do
not depend on the real checkout or on any sibling AQ project being present.
"""

import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import unittest

CHECKER = pathlib.Path(__file__).with_name("check-docs")


class CheckDocsTests(unittest.TestCase):
    def setUp(self):
        self.parent = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.parent)
        # The checkout directory name is deliberately not "aq-git", like an AQ worktree slot.
        self.root = self.parent / "slot-1"
        (self.root / "scripts").mkdir(parents=True)
        shutil.copy(CHECKER, self.root / "scripts" / "check-docs")
        (self.root / "protocol").mkdir()
        (self.root / "benchmarks" / "fixtures").mkdir(parents=True)
        (self.root / "benchmarks" / "fixtures" / "reference.json").write_text("{}")
        (self.root / "docs" / "specs").mkdir(parents=True)
        (self.root / "docs" / "api.md").write_text("# API\n")
        (self.root / "README.md").write_text("[API](docs/api.md)\n")

    def write(self, relative, text):
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def run_checker(self, **env):
        return subprocess.run(
            [sys.executable, str(self.root / "scripts" / "check-docs")],
            capture_output=True,
            text=True,
            env={**os.environ, **env},
        )

    def assertPasses(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def assertFailsWith(self, result, fragment):
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn(fragment, result.stderr)

    def test_sibling_links_pass_without_sibling_checkouts(self):
        self.write(
            "docs/aq-integration.md",
            "[eco](../../aq-server/docs/architecture/0001-ecosystem-and-releases.md#owners)\n",
        )
        self.write("docs/specs/spec.md", "[worker](../../../aq-worker/docs/specs/0001.md)\n")
        self.write("README.md", "[graph](../aq-graph/README.md) [api](docs/api.md)\n")
        self.assertPasses(self.run_checker(CHECK_DOCS_SIBLINGS=""))

    def test_in_repository_broken_link_still_fails(self):
        self.write("docs/guide.md", "[missing](missing.md)\n")
        self.assertFailsWith(self.run_checker(), "broken link missing.md")

    def test_readme_links_are_checked(self):
        self.write("README.md", "[missing](docs/missing.md)\n")
        self.assertFailsWith(self.run_checker(), "broken link docs/missing.md")

    def test_unknown_sibling_repository_fails(self):
        self.write("docs/guide.md", "[typo](../../aq-sever/README.md)\n")
        self.assertFailsWith(self.run_checker(), "unknown sibling repository")

    def test_wrong_depth_cross_repository_link_fails(self):
        # One ".." too few: resolves inside this repository rather than to a sibling.
        self.write("docs/guide.md", "[shallow](../aq-server/README.md)\n")
        self.assertFailsWith(self.run_checker(), "broken link ../aq-server/README.md")
        # One ".." too many: escapes the directory that holds the sibling checkouts.
        self.write("docs/guide.md", "[deep](../../../aq-server/README.md)\n")
        self.assertFailsWith(self.run_checker(), "escapes the sibling checkout directory")

    def test_sibling_repository_root_link_needs_a_path(self):
        self.write("docs/guide.md", "[bare](../..)\n")
        self.assertFailsWith(self.run_checker(), "must name a path in a sibling repository")
        self.write("docs/guide.md", "[repo](../../aq-server)\n")
        self.assertPasses(self.run_checker())

    def test_strict_sibling_mode_requires_targets(self):
        self.write("docs/guide.md", "[eco](../../aq-server/docs/missing.md)\n")
        self.assertFailsWith(
            self.run_checker(CHECK_DOCS_SIBLINGS="1"), "broken cross-repository link"
        )
        target = self.parent / "aq-server" / "docs" / "missing.md"
        target.parent.mkdir(parents=True)
        target.write_text("# present\n")
        self.assertPasses(self.run_checker(CHECK_DOCS_SIBLINGS="1"))

    def test_unclosed_fence_fails(self):
        self.write("docs/guide.md", "```sh\necho\n")
        self.assertFailsWith(self.run_checker(), "unclosed code fence")


if __name__ == "__main__":
    unittest.main()
