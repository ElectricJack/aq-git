#!/usr/bin/python3
"""Refresh one central aq-git installation token, restricted to its repository."""

import base64
from datetime import datetime, timedelta, timezone
from http.client import HTTPException
import json
import os
from pathlib import Path
import re
from typing import NamedTuple
import subprocess
import sys
import tempfile
import time
from urllib.error import HTTPError
from urllib.request import HTTPRedirectHandler, ProxyHandler, Request, build_opener


TOKEN_FILE = "aq-graph.token"
AQ_GIT_UID = AQ_GIT_GID = 10001


class RefreshError(Exception):
    pass


class Repository(NamedTuple):
    id: int
    full_name: str


class Installation(NamedTuple):
    client_id: str
    id: str
    key_path: Path


class TokenFile(NamedTuple):
    path: Path
    uid: int
    gid: int


GRAPH = Repository(1440134880, "ElectricJack/aq-graph")


def configured_repository(env):
    name = env.get("AQ_GITHUB_REPOSITORY")
    number = env.get("AQ_GITHUB_REPOSITORY_ID")
    if name is None and number is None:
        return GRAPH, TOKEN_FILE
    valid_name = re.fullmatch(r"[A-Za-z0-9-]+/[A-Za-z0-9_.-]+", name or "")
    valid_number = re.fullmatch(r"[1-9][0-9]{0,18}", number or "")
    if not (valid_name and valid_number):
        raise RefreshError("set both the owner/repository name and its numeric GitHub repository ID")
    return Repository(int(number), name), "repository.token"


class NoRedirects(HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def jwt(client_id, key_path):
    def encode(data):
        return base64.urlsafe_b64encode(data).rstrip(b"=")

    now = int(time.time())
    claims = json.dumps({"iat": now - 60, "exp": now + 480, "iss": client_id}).encode()
    payload = encode(b'{"alg":"RS256","typ":"JWT"}') + b"." + encode(claims)
    try:
        signature = subprocess.run(
            ["/usr/bin/openssl", "dgst", "-sha256", "-sign", str(key_path)],
            input=payload, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            check=True, timeout=5,
        ).stdout
    except (OSError, subprocess.SubprocessError):
        raise RefreshError("GitHub App JWT signing failed") from None
    return (payload + b"." + encode(signature)).decode("ascii")


def installation_token_request(installation, repository):
    client_id, installation_id, key_path = installation
    if not re.fullmatch(r"[A-Za-z0-9.]{1,128}", client_id):
        raise RefreshError("invalid GitHub App client ID")
    if not re.fullmatch(r"[1-9][0-9]{0,18}", installation_id):
        raise RefreshError("invalid GitHub App installation ID")
    return Request(
        f"https://api.github.com/app/installations/{installation_id}/access_tokens",
        method="POST",
        data=json.dumps({
            "repository_ids": [repository.id], "permissions": {"contents": "write"},
        }).encode(),
        headers={
            "Authorization": f"Bearer {jwt(client_id, key_path)}",
            "Accept": "application/vnd.github+json",
            "Content-Type": "application/json",
            "X-GitHub-Api-Version": "2026-03-10",
            "User-Agent": "aq-git-app-token-refresh",
        },
    )


def request_token(opener, request):
    try:
        with opener.open(request, timeout=15) as response:
            if response.status != 201:
                raise RefreshError("GitHub did not create an installation token")
            return response.read(65_537)
    except HTTPError as error:
        error.close()
        raise RefreshError(f"GitHub installation token request returned HTTP {error.code}") from None
    except (HTTPException, OSError):
        raise RefreshError("GitHub installation token request failed") from None


def validate_repository(repositories, repository):
    granted, = repositories
    if Repository(granted["id"], granted["full_name"]) != repository:
        raise ValueError


def validate_permissions(permissions):
    if permissions not in ({"contents": "write"}, {"contents": "write", "metadata": "read"}):
        raise ValueError


def validate_grant(body, repository):
    try:
        if len(body) > 65_536:
            raise ValueError
        grant = json.loads(body)
        token = grant["token"]
        expires = datetime.fromisoformat(grant["expires_at"])
        repositories = grant["repositories"]
        permissions = grant["permissions"]
        if not re.fullmatch(r"[!-~]{1,4096}", token):
            raise ValueError
        if expires <= datetime.now(timezone.utc) + timedelta(minutes=20):
            raise ValueError
        validate_repository(repositories, repository)
        validate_permissions(permissions)
    except (AttributeError, IndexError, KeyError, RecursionError, TypeError, ValueError):
        raise RefreshError("GitHub returned an invalid token, lifetime, or repository grant") from None
    return token, expires


def install_token(destination, token):
    token_path, uid, gid = destination
    try:
        # A temporary file on the same filesystem starts private. Readers of
        # the published path see the old complete token or the new complete one.
        with tempfile.NamedTemporaryFile(dir=token_path.parent, delete=False) as staged:
            path = staged.name
            try:
                staged.write(token.encode("ascii") + b"\n")
                staged.flush()
                os.fchmod(staged.fileno(), 0o600)
                os.fchown(staged.fileno(), uid, gid)
                os.fsync(staged.fileno())
                os.replace(path, token_path)
            finally:
                Path(path).unlink(missing_ok=True)
    except OSError:
        raise RefreshError("could not install the aq-git token file") from None


def refresh(installation, destination, opener, repository=GRAPH):
    request = installation_token_request(installation, repository)
    token, expires = validate_grant(request_token(opener, request), repository)
    install_token(destination, token)
    return expires


def main():
    try:
        repository, filename = configured_repository(os.environ)
        expires = refresh(
            Installation(
                os.environ["AQ_GITHUB_APP_CLIENT_ID"],
                os.environ["AQ_GITHUB_APP_INSTALLATION_ID"],
                Path(os.environ["CREDENTIALS_DIRECTORY"]) / "app-key.pem",
            ),
            TokenFile(Path(os.environ["STATE_DIRECTORY"]) / filename, AQ_GIT_UID, AQ_GIT_GID),
            build_opener(ProxyHandler({}), NoRedirects()),
            repository,
        )
    except KeyError:
        print("aq-git GitHub App token refresh failed: service configuration is missing", file=sys.stderr)
        return 1
    except RefreshError as error:
        print(f"aq-git GitHub App token refresh failed: {error}", file=sys.stderr)
        return 1
    print(f"aq-git GitHub App token refreshed; expires {expires.isoformat()}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
