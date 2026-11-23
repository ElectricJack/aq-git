use super::*;
pub(super) const SPAWN_HELPER_MARKER: &str = "__aq_git_spawn_v1";
const SPAWN_HELPER_FAILURE: &[u8] = b"\0aq-git-spawn-v1\0";
pub(crate) fn decode_helper_failure(nonce: &[u8; 16], bytes: &[u8]) -> Option<Error> {
    if bytes.len() != SPAWN_HELPER_FAILURE.len() + 17 {
        return None;
    }
    let rest = bytes.strip_prefix(SPAWN_HELPER_FAILURE)?;
    if &rest[..16] != nonce || !b"1234".contains(&rest[16]) {
        return None;
    }
    let error = Error::new(
        if rest[16] == b'2' {
            "unsupported_feature"
        } else {
            "git_spawn"
        },
        "internal spawn helper refused Git execution",
    );
    Some(if rest[16] == b'2' {
        error.details(serde_json::json!({"restart_required":true}))
    } else {
        error
    })
}
fn helper_fail(nonce: Option<&[u8; 16]>, code: u8) -> i32 {
    // A single bounded machine frame, bound to this spawn and not forwarded to
    // Git. Static stack storage only, including after resource limits apply.
    let mut frame = [0u8; SPAWN_HELPER_FAILURE.len() + 17];
    frame[..SPAWN_HELPER_FAILURE.len()].copy_from_slice(SPAWN_HELPER_FAILURE);
    if let Some(nonce) = nonce {
        frame[SPAWN_HELPER_FAILURE.len()..SPAWN_HELPER_FAILURE.len() + 16].copy_from_slice(nonce);
    }
    frame[SPAWN_HELPER_FAILURE.len() + 16] = code;
    unsafe {
        libc::write(libc::STDERR_FILENO, frame.as_ptr().cast(), frame.len());
    }
    126
}
/// Hidden same-binary entry, called before CLI parsing, runtime or root ownership.
/// Returns None for every ordinary invocation. Successful execution never returns.
pub fn run_spawn_helper_if_requested() -> Option<i32> {
    let mut arguments = std::env::args_os();
    let executable = arguments.next();
    let marker = arguments.next();
    if marker.as_deref() != Some(std::ffi::OsStr::new(SPAWN_HELPER_MARKER)) {
        return None;
    }
    let mut args = vec![executable.unwrap_or_default(), marker.unwrap()];
    args.extend(arguments);
    Some(run_helper(&args))
}
fn run_helper(args: &[std::ffi::OsString]) -> i32 {
    use std::os::unix::ffi::OsStrExt;
    if args.len() < 7 {
        return helper_fail(None, b'1');
    }
    let token = match args[5].to_str().and_then(|s| uuid::Uuid::parse_str(s).ok()) {
        Some(token) => token,
        None => return helper_fail(None, b'1'),
    };
    let nonce = Some(token.as_bytes());
    let Some((memory, file_limit, expected)) = helper_config(args) else {
        return helper_fail(nonce, b'1');
    };
    let git = Path::new(&args[6]);
    if !git.is_absolute() || ExecutableIdentity::read(git).ok().as_ref() != Some(&expected) {
        return helper_fail(nonce, b'2');
    }
    let Some(argv) = args[6..]
        .iter()
        .map(|arg| std::ffi::CString::new(arg.as_bytes()).ok())
        .collect::<Option<Vec<_>>>()
    else {
        return helper_fail(nonce, b'1');
    };
    let Some(environment) = helper_environment() else {
        return helper_fail(nonce, b'1');
    };
    execute_limited(&argv, &environment, (memory, file_limit), nonce)
}
fn execute_limited(
    argv: &[std::ffi::CString],
    environment: &[std::ffi::CString],
    (memory, file_limit): (u64, Option<u64>),
    nonce: Option<&[u8; 16]>,
) -> ! {
    let mut argv_ptr = argv.iter().map(|s| s.as_ptr()).collect::<Vec<_>>();
    argv_ptr.push(std::ptr::null());
    let mut env_ptr = environment.iter().map(|s| s.as_ptr()).collect::<Vec<_>>();
    env_ptr.push(std::ptr::null());
    // Everything that can allocate is prepared. Limits apply BEFORE actual Git
    // exec, without a parent-side prlimit race or an unsafe custom vfork callback.
    unsafe {
        let bound = libc::rlimit {
            rlim_cur: memory,
            rlim_max: memory,
        };
        if libc::setrlimit(libc::RLIMIT_AS, &bound) != 0 {
            let status = helper_fail(nonce, b'3');
            libc::_exit(status);
        }
        if let Some(size) = file_limit {
            let bound = libc::rlimit {
                rlim_cur: size,
                rlim_max: size,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &bound) != 0 {
                let status = helper_fail(nonce, b'3');
                libc::_exit(status);
            }
        }
        if libc::signal(libc::SIGPIPE, libc::SIG_DFL) == libc::SIG_ERR {
            let status = helper_fail(nonce, b'3');
            libc::_exit(status);
        }
        libc::execve(argv[0].as_ptr(), argv_ptr.as_ptr(), env_ptr.as_ptr());
        let status = helper_fail(nonce, b'4');
        libc::_exit(status);
    }
}
fn helper_config(args: &[std::ffi::OsString]) -> Option<(u64, Option<u64>, ExecutableIdentity)> {
    let memory = args[2].to_str()?.parse().ok().filter(|n| *n > 0)?;
    let file = if args[3] == "none" {
        None
    } else {
        Some(args[3].to_str()?.parse().ok()?)
    };
    let identity = serde_json::from_str(args[4].to_str()?).ok()?;
    Some((memory, file, identity))
}
fn helper_environment() -> Option<Vec<std::ffi::CString>> {
    use std::os::unix::ffi::OsStrExt;
    std::env::vars_os()
        .map(|(key, value)| {
            let mut entry = key.as_bytes().to_vec();
            entry.push(b'=');
            entry.extend_from_slice(value.as_bytes());
            std::ffi::CString::new(entry).ok()
        })
        .collect()
}
