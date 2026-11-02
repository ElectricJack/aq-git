//! Administrator-owned, file-backed credentials. Never serialize credential material.
use super::GitOutput;
use crate::{Error, Result, repository::Binding, state::StateRoot};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Profile {
    SshDeployKey {
        remote_url: String,
        private_key_path: PathBuf,
        known_hosts_path: PathBuf,
    },
    HttpsToken {
        remote_url: String,
        token_path: PathBuf,
    },
}

pub(crate) struct RemoteAuth {
    ssh_command: Option<String>,
    config: Vec<String>,
    material: Vec<u8>,
}
fn invalid() -> Error {
    Error::new(
        "invalid_configuration",
        "Remote credential profile is invalid or unavailable",
    )
}
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
fn path_text(path: &Path) -> Result<&str> {
    let value = path.to_str().ok_or_else(invalid)?;
    if !path.is_absolute() || value.chars().any(char::is_control) {
        return Err(invalid());
    }
    Ok(value)
}
fn read_file(path: &Path, secret: bool) -> Result<Vec<u8>> {
    path_text(path)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| invalid())?;
    let meta = file.metadata().map_err(|_| invalid())?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & if secret { 0o077 } else { 0o022 } != 0
    {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    file.take(1_048_577)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid())?;
    if bytes.is_empty() || bytes.len() > 1_048_576 {
        return Err(invalid());
    }
    Ok(bytes)
}
pub(crate) fn validate_name(value: &serde_json::Value) -> Result<()> {
    if value.is_null() {
        return Ok(());
    }
    let name = value.as_str().ok_or_else(invalid)?;
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    {
        return Err(invalid());
    }
    Ok(())
}
impl RemoteAuth {
    pub(crate) fn load(root: &StateRoot, binding: &Binding) -> Result<Self> {
        let mut auth = Self {
            ssh_command: None,
            config: vec!["http.followRedirects=false".into()],
            material: Vec::new(),
        };
        let Some(name) = &binding.credential_profile else {
            if binding
                .remote_url
                .as_deref()
                .is_some_and(|url| url.starts_with("ssh://"))
            {
                return Err(Error::new(
                    "unsupported_feature",
                    "SSH remotes require a named credential profile",
                ));
            }
            return Ok(auth);
        };
        validate_name(&serde_json::Value::String(name.clone()))?;
        if binding.mode != "central" {
            return Err(invalid());
        }
        let profiles: BTreeMap<String, Profile> = root
            .read_json("credential_profiles.json")
            .map_err(|_| invalid())?
            .unwrap_or_default();
        let profile = profiles.get(name).ok_or_else(|| {
            Error::new(
                "unsupported_feature",
                "Named credential profile is not configured",
            )
        })?;
        let url = binding.remote_url.as_deref().ok_or_else(invalid)?;
        match profile {
            Profile::SshDeployKey {
                remote_url,
                private_key_path,
                known_hosts_path,
            } => {
                if url != remote_url || !url.starts_with("ssh://") {
                    return Err(invalid());
                }
                auth.material = read_file(private_key_path, true)?;
                read_file(known_hosts_path, false)?;
                let known_hosts_option = format!(
                    "UserKnownHostsFile=\"{}\"",
                    path_text(known_hosts_path)?
                        .replace('\\', "\\\\")
                        .replace('\"', "\\\"")
                        .replace('%', "%%")
                );
                auth.ssh_command = Some(format!(
                    "/usr/bin/ssh -F /dev/null -o BatchMode=yes -o IdentitiesOnly=yes -o IdentityAgent=none -o StrictHostKeyChecking=yes -o GlobalKnownHostsFile=/dev/null -o {} -o PasswordAuthentication=no -o KbdInteractiveAuthentication=no -o UpdateHostKeys=no -i {}",
                    quote(&known_hosts_option),
                    quote(&path_text(private_key_path)?.replace('%', "%%"))
                ));
            }
            Profile::HttpsToken {
                remote_url,
                token_path,
            } => {
                if url != remote_url || !url.starts_with("https://") || url.contains(['?', '#']) {
                    return Err(invalid());
                }
                let (host, path) = url[8..].split_once('/').ok_or_else(invalid)?;
                if host.is_empty() || host.contains('@') || path.is_empty() {
                    return Err(invalid());
                }
                auth.material = read_file(token_path, true)?;
                // Tokens are one nonempty printable line, with an optional final LF.
                if auth.material.last() == Some(&b'\n') {
                    auth.material.pop();
                }
                if auth.material.is_empty()
                    || auth.material.len() > 65_536
                    || !auth.material.iter().all(|b| b.is_ascii_graphic())
                {
                    return Err(invalid());
                }
                // Git appends get/store/erase. Only get reads the file; nothing is persisted.
                // No token is interpolated into this program or its environment.
                let helper = format!(
                    "!f() {{ [ \"$1\" = get ] || exit 0; protocol= host= path=; while IFS='=' read -r key value; do [ -n \"$key\" ] || break; case \"$key\" in protocol) protocol=$value;; host) host=$value;; path) path=$value;; esac; done; [ \"$protocol\" = https ] && [ \"$host\" = {} ] && [ \"$path\" = {} ] || exit 0; token=; IFS= read -r token < {} || [ -n \"$token\" ] || exit 1; printf 'username=x-access-token\\npassword=%s\\n' \"$token\"; }}; f",
                    quote(host),
                    quote(path),
                    quote(path_text(token_path)?)
                );
                auth.config.extend([
                    "credential.useHttpPath=true".into(),
                    format!("credential.helper={helper}"),
                ]);
            }
        }
        Ok(auth)
    }
    pub(crate) fn configure(&self, command: &mut Command) {
        // Environment::configure has already cleared all inherited tracing and helpers.
        if let Some(ssh) = &self.ssh_command {
            command
                .env("GIT_SSH_COMMAND", ssh)
                .env("GIT_SSH_VARIANT", "ssh");
        }
    }
    pub(crate) fn arguments(&self, args: &[String]) -> Vec<String> {
        self.config
            .iter()
            .flat_map(|c| ["-c".into(), c.clone()])
            .chain(args.iter().cloned())
            .collect()
    }
    pub(crate) fn finish(&self, mut output: GitOutput) -> Result<GitOutput> {
        let diagnostic = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
        let denied = [
            "permission denied (",
            "authentication failed",
            "host key verification failed",
            "remote host identification has changed",
            "could not read username",
            "could not read password",
            "http basic: access denied",
            "requested url returned error: 401",
            "requested url returned error: 403",
            "repository not found",
        ]
        .iter()
        .any(|text| diagnostic.contains(text));
        if !output.status.success() && denied {
            return Err(Error::new(
                "remote_auth_failed",
                "Remote authentication or host verification failed",
            ));
        }
        // Remote diagnostic text is never part of any response, journal or debug output.
        output.stderr.clear();
        // Defense in depth for a helper/remote that echoes credential material to stdout.
        if !self.material.is_empty() {
            for secret in self
                .material
                .split(|b| *b == b'\n')
                .filter(|s| !s.is_empty())
            {
                let mut start = 0;
                while let Some(offset) = output.stdout[start..]
                    .windows(secret.len())
                    .position(|w| w == secret)
                {
                    let at = start + offset;
                    output
                        .stdout
                        .splice(at..at + secret.len(), b"[REDACTED]".iter().copied());
                    start = at + 10;
                }
            }
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{io::Write, os::unix::fs::PermissionsExt, process::Stdio};

    #[test]
    fn https_helper_is_scoped_get_only_and_not_persistent() {
        let temp = tempfile::tempdir().unwrap();
        let token = temp.path().join("token ' with spaces");
        std::fs::write(&token, "synthetic-token\n").unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
        let root = StateRoot::open(temp.path().join("state")).unwrap();
        root.write_json("credential_profiles.json", &json!({"token":{"kind":"https_token", "remote_url":"https://github.com/org/repo.git", "token_path":token}})).unwrap();
        let binding: Binding = serde_json::from_value(json!({"repository_id":uuid::Uuid::new_v4(),"binding_generation":1,"identity":"fixture","mode":"central","object_format":"sha1","remote_url":"https://github.com/org/repo.git","credential_profile":"token"})).unwrap();
        let auth = RemoteAuth::load(&root, &binding).unwrap();
        // Exercise the actual helper via Git's credential protocol, including its
        // config reset and URL matching, without introducing a network dependency.
        let invoke = |operation: &str, input: &[u8], authenticated: bool| {
            let args = ["credential".into(), operation.into()];
            let mut command = Command::new("/usr/bin/git");
            super::super::environment::configure(&mut command);
            command.args(if authenticated {
                auth.arguments(&args)
            } else {
                args.to_vec()
            });
            command
                .current_dir(temp.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            let mut child = command.spawn().unwrap();
            child.stdin.take().unwrap().write_all(input).unwrap();
            child.wait_with_output().unwrap()
        };
        let input = b"protocol=https\nhost=github.com\npath=org/repo.git\n\n";
        let output = invoke("fill", input, true);
        assert!(output.status.success());
        assert!(
            String::from_utf8(output.stdout)
                .unwrap()
                .contains("password=synthetic-token\n")
        );
        for input in [
            b"protocol=https\nhost=other.test\npath=org/repo.git\n\n".as_slice(),
            b"protocol=https\nhost=github.com\npath=other/repo.git\n\n".as_slice(),
            b"protocol=http\nhost=github.com\npath=org/repo.git\n\n".as_slice(),
        ] {
            let output = invoke("fill", input, true);
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
        }
        assert!(invoke("approve", input, true).status.success());
        assert!(invoke("reject", input, true).status.success());
        assert!(!invoke("fill", input, false).status.success());
        assert_eq!(
            std::fs::read_to_string(&token).unwrap(),
            "synthetic-token\n"
        );
    }
}
