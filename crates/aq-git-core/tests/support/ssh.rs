//! Real loopback SSH transport. Requires OpenSSH sshd (AQ_TEST_SSHD may override).
use serde_json::{Value, json};
use std::{
    fs,
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

pub struct SshRemote {
    pub temp: tempfile::TempDir,
    child: Child,
    port: u16,
    user: String,
}
impl SshRemote {
    pub fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for name in ["host", "client", "wrong"] {
            let output = Command::new("/usr/bin/ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(root.join(name))
                .output()
                .unwrap();
            assert!(output.status.success(), "ssh-keygen failed");
        }
        fs::copy(root.join("client.pub"), root.join("authorized_keys")).unwrap();
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_owned();
        fs::write(
            root.join("known hosts ' pinned"),
            format!(
                "[127.0.0.1]:{port} {}",
                fs::read_to_string(root.join("host.pub")).unwrap()
            ),
        )
        .unwrap();
        fs::write(root.join("sshd_config"), format!("ListenAddress 127.0.0.1\nPort {port}\nHostKey {}\nPidFile {}\nAuthorizedKeysFile {}\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nAllowUsers {user}\nLogLevel ERROR\n", root.join("host").display(),root.join("pid").display(),root.join("authorized_keys").display())).unwrap();
        let sshd = std::env::var_os("AQ_TEST_SSHD")
            .map(PathBuf::from)
            .unwrap_or_else(|| "/usr/sbin/sshd".into());
        let log = fs::File::create(root.join("sshd.log")).unwrap();
        let child = Command::new(sshd)
            .args(["-D", "-e", "-f"])
            .arg(root.join("sshd_config"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .expect("install OpenSSH server or set AQ_TEST_SSHD to an absolute sshd path");
        let mut fixture = Self {
            temp,
            child,
            port,
            user,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            assert!(
                fixture.child.try_wait().unwrap().is_none(),
                "sshd exited: {}",
                fs::read_to_string(fixture.temp.path().join("sshd.log")).unwrap()
            );
            assert!(Instant::now() < deadline, "sshd startup timeout");
            std::thread::sleep(Duration::from_millis(10));
        }
        fixture
    }
    pub fn url(&self, remote: &Path) -> String {
        format!(
            "ssh://{}@127.0.0.1:{}{}",
            self.user,
            self.port,
            remote.display()
        )
    }
    pub fn profile(&self, remote: &Path) -> Value {
        json!({"kind":"ssh_deploy_key","remote_url":self.url(remote),"private_key_path":self.temp.path().join("client"),"known_hosts_path":self.temp.path().join("known hosts ' pinned")})
    }
    pub fn deny_key(&self) {
        fs::copy(
            self.temp.path().join("wrong.pub"),
            self.temp.path().join("authorized_keys"),
        )
        .unwrap();
    }
    pub fn wrong_host(&self) {
        fs::write(
            self.temp.path().join("known hosts ' pinned"),
            format!(
                "[127.0.0.1]:{} {}",
                self.port,
                fs::read_to_string(self.temp.path().join("wrong.pub")).unwrap()
            ),
        )
        .unwrap();
    }
}
impl Drop for SshRemote {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
