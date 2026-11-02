use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader},
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, Command, Stdio},
};

pub struct HttpsRemote {
    pub temp: tempfile::TempDir,
    child: Child,
    port: u16,
}
impl HttpsRemote {
    pub fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let cert = Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=127.0.0.1",
                "-addext",
                "subjectAltName=IP:127.0.0.1",
                "-keyout",
            ])
            .arg(temp.path().join("key.pem"))
            .arg("-out")
            .arg(temp.path().join("cert.pem"))
            .output()
            .unwrap();
        assert!(
            cert.status.success(),
            "fixture TLS certificate generation failed"
        );
        fs::write(temp.path().join("token"), "synthetic_https_fixture_token").unwrap();
        fs::set_permissions(temp.path().join("token"), fs::Permissions::from_mode(0o600)).unwrap();
        let mut child = Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/support/https_server.py"
            ))
            .arg(temp.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let port = line
            .trim()
            .parse()
            .expect("HTTPS fixture must report its bound port");
        Self { temp, child, port }
    }
    pub fn attach(&self, remote: &Path) {
        // The server serves a private link to the fixture's bare repository.
        std::os::unix::fs::symlink(remote, self.temp.path().join("repo.git")).unwrap();
    }
    pub fn url(&self) -> String {
        format!("https://127.0.0.1:{}/repo.git", self.port)
    }
    pub fn profile(&self) -> Value {
        json!({"kind":"https_token","remote_url":self.url(),"token_path":self.temp.path().join("token")})
    }
    pub fn runner(&self) -> aq_git_core::git::GitRunner {
        // Trust only this test certificate while preserving real HTTPS validation
        // and the production helper. No sslVerify=false or URL rewrite.
        let wrapper = self.temp.path().join("git-with-fixture-ca");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nexec /usr/bin/git -c http.sslCAInfo='{}' \"$@\"\n",
                self.temp.path().join("cert.pem").display()
            ),
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        aq_git_core::git::GitRunner::new(wrapper)
    }
}
impl Drop for HttpsRemote {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
