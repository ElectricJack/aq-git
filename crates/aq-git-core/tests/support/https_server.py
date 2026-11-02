"""Loopback HTTPS Git fixture; credentials stay in temporary private files."""
import base64
import http.server
import os
from pathlib import Path
import ssl
import subprocess
import sys

root = Path(sys.argv[1])


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        self.serve_git()

    def do_POST(self):
        self.serve_git()

    def serve_git(self):
        expected = "Basic " + base64.b64encode(
            b"x-access-token:" + (root / "token").read_bytes().strip()
        ).decode()
        if self.headers.get("Authorization") != expected:
            self.send_response(401)
            self.send_header("WWW-Authenticate", 'Basic realm="fixture"')
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        path, _, query = self.path.partition("?")
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length)
        env = {
            "PATH": "/usr/bin:/bin",
            "HOME": "/nonexistent",
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_PROJECT_ROOT": str(root),
            "GIT_HTTP_EXPORT_ALL": "1",
            "PATH_INFO": path,
            "QUERY_STRING": query,
            "REQUEST_METHOD": self.command,
            "CONTENT_TYPE": self.headers.get("Content-Type", ""),
            "CONTENT_LENGTH": str(length),
            "REMOTE_USER": "fixture",
            "REMOTE_ADDR": "127.0.0.1",
        }
        output = subprocess.run(
            ["/usr/bin/git", "http-backend"], input=body,
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env, check=True,
        ).stdout
        headers, body = output.split(b"\r\n\r\n", 1)
        fields = [line.decode().split(":", 1) for line in headers.split(b"\r\n")]
        status = next((int(value.strip().split()[0]) for key, value in fields if key.lower() == "status"), 200)
        self.send_response(status)
        for key, value in fields:
            if key.lower() != "status":
                self.send_header(key, value.strip())
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(root / "cert.pem", root / "key.pem")
server.socket = context.wrap_socket(server.socket, server_side=True)
print(server.server_port, flush=True)
server.serve_forever()
