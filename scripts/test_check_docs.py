"""Regression tests for scripts/check-docs (standard library only)."""

import contextlib
import importlib.machinery
import importlib.util
import io
import pathlib
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).with_name("check-docs")
loader = importlib.machinery.SourceFileLoader("check_docs", str(SCRIPT))
spec = importlib.util.spec_from_loader("check_docs", loader)
check_docs = importlib.util.module_from_spec(spec)
loader.exec_module(check_docs)


class CheckDocsTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        # <workspace>/aq-git is the checkout; siblings live beside it.
        self.workspace = pathlib.Path(self.temp.name)
        self.root = self.workspace / "aq-git"
        self.write("docs/local.md", "# Local\n")
        self.write("protocol/schema.json", "{}")
        self.write("benchmarks/fixtures/reference.json", "{}")

    def write(self, relative, text, base=None):
        path = (base or self.root) / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        return path

    def run_check(self, *args):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = check_docs.main(["--root", str(self.root), *args])
        return code, out.getvalue(), err.getvalue()

    def test_absent_sibling_checkout_is_reported_not_failed(self):
        self.write(
            "docs/guide.md",
            "[local](local.md) [server](../../aq-server/docs/a.md#top) "
            "[readme](../../aq-worker/README.md)\n",
        )
        code, out, err = self.run_check()
        self.assertEqual(code, 0, err)
        self.assertIn("2 cross-repository links not checked", out)
        self.assertIn("aq-server", out)

    def test_present_sibling_checkout_is_validated(self):
        self.write("docs/a.md", "# A\n", base=self.workspace / "aq-server")
        self.write("docs/guide.md", "[ok](../../aq-server/docs/a.md)\n")
        code, out, err = self.run_check()
        self.assertEqual(code, 0, err)
        self.assertIn("1 cross-repository link checked", out)

        self.write("docs/guide.md", "[gone](../../aq-server/docs/missing.md)\n")
        code, _, err = self.run_check()
        self.assertEqual(code, 1)
        self.assertIn("broken cross-repository link ../../aq-server/docs/missing.md", err)

    def test_sibling_root_override_locates_siblings_elsewhere(self):
        elsewhere = self.workspace / "elsewhere"
        self.write("docs/a.md", "# A\n", base=elsewhere / "aq-server")
        self.write("docs/guide.md", "[gone](../../aq-server/docs/missing.md)\n")
        code, _, err = self.run_check("--sibling-root", str(elsewhere))
        self.assertEqual(code, 1)
        self.assertIn(str(elsewhere / "aq-server"), err)

    def test_require_siblings_fails_when_checkout_absent(self):
        self.write("docs/guide.md", "[server](../../aq-server/docs/a.md)\n")
        code, _, err = self.run_check("--require-siblings")
        self.assertEqual(code, 1)
        self.assertIn("sibling checkout aq-server not found", err)

    def test_unknown_or_malformed_external_links_fail(self):
        for link in (
            "../../aq-sever/docs/a.md",  # typo of a known sibling
            "../../../elsewhere/a.md",  # escapes above the sibling root
            "../..",  # the sibling root itself
        ):
            with self.subTest(link=link):
                self.write("docs/guide.md", f"[bad]({link})\n")
                code, _, err = self.run_check()
                self.assertEqual(code, 1)
                self.assertIn(f"link {link} leaves the repository", err)

    def test_broken_local_link_fails_and_all_errors_are_reported(self):
        self.write("docs/guide.md", "[one](missing.md) [two](nested/also.md#x)\n")
        self.write("README.md", "[three](docs/nope.md)\n")
        code, _, err = self.run_check()
        self.assertEqual(code, 1)
        self.assertIn("broken link missing.md", err)
        self.assertIn("broken link nested/also.md#x", err)
        self.assertIn("README.md: broken link docs/nope.md", err)

    def test_ignored_link_forms(self):
        self.write(
            "docs/guide.md",
            "[web](https://example.com/x) [anchor](#here) [mail](mailto:a@b.c)\n",
        )
        code, _, err = self.run_check()
        self.assertEqual(code, 0, err)

    def test_unclosed_fence_and_invalid_json_fail(self):
        self.write("docs/guide.md", "```sh\necho\n")
        self.write("protocol/bad.json", "{")
        code, _, err = self.run_check()
        self.assertEqual(code, 1)
        self.assertIn("unclosed code fence", err)
        self.assertIn("bad.json: invalid JSON", err)

    def test_repository_documents_pass_without_sibling_checkouts(self):
        out, err = io.StringIO(), io.StringIO()
        with tempfile.TemporaryDirectory() as empty:
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                code = check_docs.main(["--sibling-root", empty])
        self.assertEqual(code, 0, err.getvalue())
        self.assertIn("cross-repository links not checked", out.getvalue())


if __name__ == "__main__":
    unittest.main()
