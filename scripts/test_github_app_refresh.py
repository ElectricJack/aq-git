import base64
from datetime import datetime, timedelta, timezone
import importlib.util
import io
import json
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import unittest
from unittest.mock import Mock, patch
from urllib.error import HTTPError


helper = Path(__file__).resolve().parents[1] / "deploy/github-app/refresh.py"
spec = importlib.util.spec_from_file_location("github_app_refresh", helper)
refresh = importlib.util.module_from_spec(spec)
spec.loader.exec_module(refresh)


class Response(io.BytesIO):
    status = 201

    def __init__(self, grant):
        super().__init__(json.dumps(grant).encode())


class RefreshTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        keys = cls.enterClassContext(tempfile.TemporaryDirectory())
        cls.key = Path(keys) / "app.pem"
        cls.public_key = Path(keys) / "app.pub"
        subprocess.run(
            ["openssl", "genpkey", "-algorithm", "RSA", "-out", cls.key],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True,
        )
        subprocess.run(
            ["openssl", "pkey", "-in", cls.key, "-pubout", "-out", cls.public_key],
            check=True,
        )

    def setUp(self):
        self.directory = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.token_path = self.directory / refresh.TOKEN_FILE
        self.opener = Mock()

    def grant(self, token):
        return {
            "token": token,
            "expires_at": (datetime.now(timezone.utc) + timedelta(hours=1)).isoformat(),
            "repositories": [{"id": 1440134880, "full_name": "ElectricJack/aq-graph"}],
            "permissions": {"contents": "write", "metadata": "read"},
        }

    def run_refresh(self, grant):
        self.opener.open.return_value = Response(grant)
        return refresh.refresh("Iv1.fixture", "321", self.key, self.token_path,
                               os.getuid(), os.getgid(), self.opener)

    def test_renews_the_same_path_with_a_repository_scoped_signed_request(self):
        self.run_refresh(self.grant("synthetic-first"))
        with self.token_path.open() as old:
            self.run_refresh(self.grant("synthetic-renewed"))
            self.assertEqual(old.read(), "synthetic-first\n")
        self.assertEqual(self.token_path.read_text(), "synthetic-renewed\n")
        info = self.token_path.stat()
        self.assertEqual(stat.S_IMODE(info.st_mode), 0o600)
        self.assertEqual((info.st_uid, info.st_gid), (os.getuid(), os.getgid()))
        self.assertEqual(list(self.directory.iterdir()), [self.token_path])

        request, = self.opener.open.call_args.args
        self.assertEqual(request.full_url, "https://api.github.com/app/installations/321/access_tokens")
        self.assertEqual(request.method, "POST")
        self.assertEqual(json.loads(request.data), {
            "repository_ids": [1440134880], "permissions": {"contents": "write"},
        })
        bearer, encoded = request.get_header("Authorization").split(" ", 1)
        self.assertEqual(bearer, "Bearer")
        header, claims, signature = encoded.split(".")

        def decode(part):
            return base64.urlsafe_b64decode(part + "=" * (-len(part) % 4))

        self.assertEqual(json.loads(decode(header))["alg"], "RS256")
        payload = json.loads(decode(claims))
        self.assertEqual(payload["iss"], "Iv1.fixture")
        self.assertLessEqual(payload["exp"] - payload["iat"], 600)
        sign_file = self.directory / "signature"
        sign_file.write_bytes(decode(signature))
        subprocess.run(
            ["openssl", "dgst", "-sha256", "-verify", self.public_key, "-signature", sign_file],
            input=f"{header}.{claims}".encode(), stdout=subprocess.DEVNULL, check=True,
        )

    def test_failed_grants_and_http_errors_preserve_the_previous_token(self):
        self.run_refresh(self.grant("synthetic-current"))
        invalid = self.grant("synthetic-sensitive-response")
        for fields in (
            {"repositories": []},
            {"repositories": [{"id": 1, "full_name": "ElectricJack/aq-graph"}]},
            {"repositories": invalid["repositories"] * 2},
            {"permissions": {"contents": "write", "issues": "write"}},
            {"permissions": {"contents": "read"}},
            {"expires_at": datetime.now(timezone.utc).isoformat()},
            {"token": "two\nlines"},
        ):
            with self.subTest(fields=fields):
                with self.assertRaisesRegex(refresh.RefreshError, "invalid token, lifetime, or repository grant"):
                    self.run_refresh({**invalid, **fields})
                self.assertEqual(self.token_path.read_text(), "synthetic-current\n")

        self.opener.open.side_effect = HTTPError(
            "https://api.github.com", 403, "sensitive detail", {}, io.BytesIO(b"sensitive body"),
        )
        with self.assertRaisesRegex(refresh.RefreshError, "HTTP 403$"):
            self.run_refresh(invalid)
        self.assertEqual(self.token_path.read_text(), "synthetic-current\n")

    def test_install_failure_leaves_the_previous_token_and_cleans_up_staging(self):
        self.run_refresh(self.grant("synthetic-current"))
        with patch.object(refresh.os, "replace", side_effect=OSError("sensitive detail")):
            with self.assertRaisesRegex(refresh.RefreshError, "could not install the aq-git token file$"):
                self.run_refresh(self.grant("synthetic-renewed"))
        self.assertEqual(self.token_path.read_text(), "synthetic-current\n")
        self.assertEqual(list(self.directory.iterdir()), [self.token_path])
