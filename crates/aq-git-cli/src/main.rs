use aq_git_core::RepositoryService;
use aq_git_core::state::StateRoot;
use aq_git_protocol::{
    MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, Request, Response, decode_request, decode_response,
};
use clap::Parser;
use serde_json::json;
use std::{path::PathBuf, sync::Arc};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
mod recovery;
mod response;
mod serving;
mod socket;
use recovery::{clear_fence, guard};
use response::{exit_code, print_response};
use serving::run_server;
use socket::client;
type CliResult<T> = Result<T, Box<dyn std::error::Error>>;

#[derive(Parser, Debug)]
#[command(name = "aq-git", version, about = "Bounded local repository service")]
struct Args {
    #[arg(long, conflicts_with = "socket")]
    state_dir: Option<PathBuf>,
    #[arg(long)]
    socket: Option<PathBuf>,
    #[arg(long)]
    input: Option<PathBuf>,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    listen: Option<PathBuf>,
    #[arg(long, default_value = "/usr/bin/git")]
    git_executable: PathBuf,
    /// Absolute operator-held credential profile file (for example a read-only
    /// vault mount); defaults to credential_profiles.json in the state root.
    #[arg(long, conflicts_with = "socket")]
    credential_profiles: Option<PathBuf>,
    /// `serve` only: a second Unix socket speaking Git smart HTTP for task
    /// attempts. Only aq-server reaches it and forwards each request with a grant.
    #[arg(long, requires = "listen")]
    git_http_socket: Option<PathBuf>,
    #[arg(required=true,num_args=1..=2)]
    command: Vec<String>,
}
fn main() {
    if let Some(status) = aq_git_core::git::run_spawn_helper_if_requested() {
        std::process::exit(status);
    }
    if let Some(status) = aq_git_core::git_http::run_hook_if_requested() {
        std::process::exit(status);
    }
    let args = Args::parse();
    let machine = args.json;
    let code = match run(args) {
        Ok(code) => code,
        Err(e) => {
            if machine {
                let response = if let Some(error) = e.downcast_ref::<aq_git_core::Error>() {
                    Response::failure("", &error.code, &error.message, error.details.clone())
                } else if e.downcast_ref::<std::io::Error>().is_some() {
                    Response::failure("", "io_error", e.to_string(), json!({}))
                } else {
                    Response::failure("", "invalid_request", e.to_string(), json!({}))
                };
                let _ = print_response(&response, true, "startup");
                exit_code(&response)
            } else {
                eprintln!("{e}");
                eprintln!(
                    "next: doctor or inspect capabilities; correct configuration or request inputs"
                );
                if let Some(error) = e.downcast_ref::<aq_git_core::Error>() {
                    exit_code(&Response::failure(
                        "",
                        &error.code,
                        &error.message,
                        error.details.clone(),
                    ))
                } else if e.downcast_ref::<std::io::Error>().is_some() {
                    1
                } else {
                    2
                }
            }
        }
    };
    std::process::exit(code);
}
fn run(args: Args) -> Result<i32, Box<dyn std::error::Error>> {
    let operation = args.command.join(".");
    if !args.git_executable.is_absolute() {
        return Err("--git-executable must be an absolute administrator-selected path".into());
    }
    if operation == "recovery.clear-fence" {
        return clear_fence(args);
    }
    let state = open_state(&args, &operation)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        // Git and core work use the separately admitted blocking executor.
        // Keep socket scheduling bounded independently of host CPU count.
        .worker_threads(aq_git_protocol::SOCKET_IO_WORKERS)
        .enable_all()
        .build()?;
    if operation == "serve" {
        return run_server(args, state, runtime);
    }
    let bytes = request_bytes(args.input.clone(), &operation)?;
    let request = match decode_request(&bytes) {
        Ok(r) => r,
        Err(e) => {
            let response = decode_failure(&bytes, e);
            return Ok(print_response(&response, args.json, &operation)?);
        }
    };
    if operation != "request" && operation != request.operation {
        return Err("Command operation differs from input envelope".into());
    }
    let machine = args.json;
    let response = execute_request(args, (&operation, request), state, runtime)?;
    Ok(print_response(&response, machine, &operation)?)
}

fn open_state(args: &Args, operation: &str) -> CliResult<Option<StateRoot>> {
    Ok(if args.socket.is_none() && operation != "doctor" {
        let root = args
            .state_dir
            .as_ref()
            .ok_or("Choose --state-dir or --socket")?;
        Some(guard(aq_git_core::state::StateRoot::open_for_recovery(
            root,
        )?)?)
    } else {
        None
    })
}

fn request_bytes(input: Option<PathBuf>, operation: &str) -> CliResult<Vec<u8>> {
    match input {
        Some(path) if path.as_os_str() != "-" => read_file(path),
        Some(_) => read_stdin(),
        None if operation == "request" => read_stdin(),
        None => Ok(serde_json::to_vec(
            &json!({"protocol_version":1,"request_id":uuid::Uuid::new_v4().to_string(),"operation":operation,"params":{}}),
        )?),
    }
}

fn execute_request(
    args: Args,
    (operation, request): (&str, Request),
    state: Option<StateRoot>,
    runtime: tokio::runtime::Runtime,
) -> CliResult<Response> {
    Ok(if operation == "doctor" && args.socket.is_none() {
        // Read-only inspection of an abandoned active fence cannot clear it or
        // replay repository transitions, and must not launch Git.
        RepositoryService::for_diagnostics(aq_git_core::state::StateRoot::open_for_recovery(
            args.state_dir.as_ref().ok_or("Choose --state-dir")?,
        )?)?
        .execute(request)
    } else if let Some(socket) = args.socket {
        runtime.block_on(client(socket, &request))?
    } else {
        direct_service(
            state.unwrap(),
            args.git_executable,
            args.credential_profiles,
        )?
        .execute(request)
    })
}
fn direct_service(
    state: StateRoot,
    executable: PathBuf,
    profiles: Option<PathBuf>,
) -> CliResult<RepositoryService> {
    let service = RepositoryService::from_state_root_with_git(state, executable)?
        .with_spawn_helper(PathBuf::from("/proc/self/exe"))?;
    match profiles {
        Some(path) => Ok(service.with_credential_profiles(path)?),
        None => Ok(service),
    }
}
fn read_file(path: PathBuf) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}
fn read_stdin() -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::io::stdin()
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn decode_failure(bytes: &[u8], e: aq_git_protocol::ProtocolError) -> Response {
    let id = serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|v| v["request_id"].as_str().map(str::to_owned))
        .unwrap_or_default();
    Response::failure(id, e.code, e.message, e.details)
}
