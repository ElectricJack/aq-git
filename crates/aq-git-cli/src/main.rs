use aq_git_core::RepositoryService;
use aq_git_protocol::{
    MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, Request, Response, decode_request, decode_response,
};
use clap::Parser;
use serde_json::json;
use std::{path::PathBuf, sync::Arc};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

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
    #[arg(required=true,num_args=1..=2)]
    command: Vec<String>,
}
fn main() {
    if let Some(status) = aq_git_core::git::run_spawn_helper_if_requested() {
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
    let state = if args.socket.is_none() && operation != "doctor" {
        let root = args
            .state_dir
            .as_ref()
            .ok_or("Choose --state-dir or --socket")?;
        Some(guard(aq_git_core::state::StateRoot::open(root)?)?)
    } else {
        None
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        // Git and core work use the separately admitted blocking executor.
        // Keep socket scheduling bounded independently of host CPU count.
        .worker_threads(aq_git_protocol::SOCKET_IO_WORKERS)
        .enable_all()
        .build()?;
    if operation == "serve" {
        let root = args
            .state_dir
            .as_ref()
            .ok_or("serve requires --state-dir")?;
        let socket = args.listen.as_ref().ok_or("serve requires --listen PATH")?;
        let _ = root;
        let service = Arc::new(
            RepositoryService::from_state_root_with_git(state.unwrap(), args.git_executable)?
                .with_spawn_helper(PathBuf::from("/proc/self/exe"))?,
        );
        runtime.block_on(serve(socket, service))?;
        return Ok(0);
    }
    let bytes = if let Some(path) = args.input {
        if path.as_os_str() == "-" {
            read_stdin()?
        } else {
            read_file(path)?
        }
    } else if operation == "request" {
        read_stdin()?
    } else {
        serde_json::to_vec(
            &json!({"protocol_version":1,"request_id":uuid::Uuid::new_v4().to_string(),"operation":operation,"params":{}}),
        )?
    };
    let request = match decode_request(&bytes) {
        Ok(r) => r,
        Err(e) => {
            let id = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|v| v["request_id"].as_str().map(str::to_owned))
                .unwrap_or_default();
            let response = Response::failure(id, e.code, e.message, e.details);
            return Ok(print_response(&response, args.json, &operation)?);
        }
    };
    if operation != "request" && operation != request.operation {
        return Err("Command operation differs from input envelope".into());
    }
    let response = if operation == "doctor" && args.socket.is_none() {
        // Read-only inspection of an abandoned active fence cannot clear it or
        // replay repository transitions, and must not launch Git.
        RepositoryService::for_diagnostics(aq_git_core::state::StateRoot::open_for_recovery(
            args.state_dir.as_ref().ok_or("Choose --state-dir")?,
        )?)?
        .execute(request)
    } else if let Some(socket) = args.socket {
        runtime.block_on(client(socket, &request))?
    } else {
        RepositoryService::from_state_root_with_git(state.unwrap(), args.git_executable)?
            .with_spawn_helper(PathBuf::from("/proc/self/exe"))?
            .execute(request)
    };
    Ok(print_response(&response, args.json, &operation)?)
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
async fn frame<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    max: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "Incomplete NDJSON frame",
                ))
            };
        }
        let count = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |n| n + 1);
        if bytes.len() + count > max + 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Frame limit exceeded",
            ));
        }
        let done = available[count - 1] == b'\n';
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if done {
            bytes.pop();
            if bytes.len() > max {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Frame limit exceeded",
                ));
            }
            return Ok(Some(bytes));
        }
    }
}
async fn client(path: PathBuf, request: &Request) -> Result<Response, Box<dyn std::error::Error>> {
    let mut stream = tokio::net::UnixStream::connect(path).await?;
    let mut bytes = serde_json::to_vec(request)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    let mut reader = BufReader::new(stream);
    let bytes = tokio::time::timeout(
        std::time::Duration::from_millis(request.deadline_ms.unwrap_or(310_000)),
        frame(&mut reader, MAX_RESPONSE_BYTES),
    )
    .await??
    .ok_or("Server disconnected")?;
    let mut response = match decode_response(&bytes) {
        Ok(response) => response,
        Err(error) => Response::error(request, error.code, error.message, error.details),
    };
    if response.request_id != request.request_id {
        response = Response::error(
            request,
            "invalid_response",
            "Response request_id mismatch",
            json!({}),
        );
    }
    if response
        .operation_id
        .as_ref()
        .is_some_and(|id| request.operation_id.as_ref() != Some(id))
    {
        response = Response::error(
            request,
            "invalid_response",
            "Response operation_id mismatch",
            json!({}),
        );
    }
    if request.is_mutation()
        && let Some(error) = &mut response.error
        && matches!(
            error.code.as_str(),
            "invalid_response" | "unsupported_protocol"
        )
    {
        if !error.details.is_object() {
            let server_details = error.details.take();
            error.details = json!({"server_details":server_details});
        }
        error.details["operation_outcome"] = json!("unknown");
        error.retry = aq_git_protocol::RetryAdvice::ReconcileFirst;
    }
    Ok(response)
}
async fn serve(
    path: &std::path::Path,
    service: Arc<RepositoryService>,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let path = path.as_path();
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if !metadata.file_type().is_socket() {
            return Err("Refusing to replace a non-socket path".into());
        }
        match tokio::time::timeout(
            std::time::Duration::from_millis(250),
            tokio::net::UnixStream::connect(path),
        )
        .await
        {
            Ok(Err(error)) if error.kind() == std::io::ErrorKind::ConnectionRefused => {}
            _ => return Err("Refusing to replace a live or unverified socket".into()),
        }
        let owner: Option<serde_json::Value> = service.root.read_json("service-socket.json")?;
        if !owner.as_ref().is_some_and(|o| {
            o["active"] == true
                && o["path"].as_str() == path.to_str()
                && o["device"].as_u64() == Some(metadata.dev())
                && o["inode"].as_u64() == Some(metadata.ino())
        }) {
            return Err("Stale socket is not recorded as owned by this state root".into());
        }
        let rechecked = std::fs::symlink_metadata(path)?;
        if rechecked.dev() != metadata.dev() || rechecked.ino() != metadata.ino() {
            return Err("Socket changed during ownership verification".into());
        }
        std::fs::remove_file(path)?;
    }
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let bound_metadata = std::fs::symlink_metadata(path)?;
    service.root.write_json("service-socket.json",&json!({"active":true,"path":path,"device":bound_metadata.dev(),"inode":bound_metadata.ino()}))?;
    let connections = Arc::new(tokio::sync::Semaphore::new(64));
    let admitted = Arc::new(tokio::sync::Semaphore::new(64));
    let reads = Arc::new(tokio::sync::Semaphore::new(32));
    let heavy = Arc::new(tokio::sync::Semaphore::new(4));
    let syncs = Arc::new(tokio::sync::Semaphore::new(32));
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _=interrupt.recv()=>break,
            _=terminate.recv()=>break,
            Some(_)=tasks.join_next(),if !tasks.is_empty()=>{},
            accepted=listener.accept()=>{
                let(stream,_)=accepted?;
                let Ok(connection)=connections.clone().try_acquire_owned() else {
                    let response=Response::failure("","overloaded","Connection limit reached",json!({}));
                    if let Ok(mut bytes)=serde_json::to_vec(&response){bytes.push(b'\n');let _=stream.try_write(&bytes);}
                    continue;
                };
                let service=service.clone(); let reads=reads.clone(); let heavy=heavy.clone();let syncs=syncs.clone();let admitted=admitted.clone();
                tasks.spawn(async move{let _connection=connection;serve_connection(stream,service,reads,heavy,syncs,admitted).await;});
            }
        }
    }
    drop(listener);
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    let current = std::fs::symlink_metadata(path)?;
    if current.dev() != bound_metadata.dev() || current.ino() != bound_metadata.ino() {
        return Err("Bound socket path was replaced; refusing to remove replacement".into());
    }
    std::fs::remove_file(path)?;
    service.root.write_json("service-socket.json",&json!({"active":false,"path":path,"device":bound_metadata.dev(),"inode":bound_metadata.ino()}))?;
    Ok(())
}
// Waiting for another request is separate from assembling a request already
// started. A quiet client with bounded work outstanding is not a stalled frame.
async fn connection_frame<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    max: usize,
    pending: &mut tokio::task::JoinSet<()>,
    idle_timeout: std::time::Duration,
    partial_timeout: std::time::Duration,
) -> std::io::Result<Option<Vec<u8>>> {
    {
        let first_byte = reader.fill_buf();
        tokio::pin!(first_byte);
        let idle = tokio::time::sleep(idle_timeout);
        tokio::pin!(idle);
        loop {
            tokio::select! {
                data=&mut first_byte=>{
                    if data?.is_empty(){return Ok(None);}
                    break;
                }
                _=pending.join_next(),if !pending.is_empty()=>{
                    if pending.is_empty(){idle.as_mut().reset(tokio::time::Instant::now()+idle_timeout);}
                }
                _=&mut idle,if pending.is_empty()=>return Ok(None),
            }
        }
    }
    // Keep this exact parser future alive while reaping responses. Dropping and
    // recreating it would lose bytes that frame() has already consumed.
    let parsing = frame(reader, max);
    tokio::pin!(parsing);
    let stalled = tokio::time::sleep(partial_timeout);
    tokio::pin!(stalled);
    loop {
        tokio::select! {
            result=&mut parsing=>return result,
            _=pending.join_next(),if !pending.is_empty()=>{},
            _=&mut stalled=>return Err(std::io::Error::new(std::io::ErrorKind::TimedOut,"Partial request frame timed out")),
        }
    }
}
async fn serve_connection(
    stream: tokio::net::UnixStream,
    service: Arc<RepositoryService>,
    reads: Arc<tokio::sync::Semaphore>,
    heavy: Arc<tokio::sync::Semaphore>,
    syncs: Arc<tokio::sync::Semaphore>,
    admitted: Arc<tokio::sync::Semaphore>,
) {
    let (r, w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let writer = Arc::new(tokio::sync::Mutex::new(w));
    let mut pending = tokio::task::JoinSet::new();
    loop {
        let bytes = match connection_frame(
            &mut reader,
            MAX_REQUEST_BYTES,
            &mut pending,
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(30),
        )
        .await
        {
            Ok(Some(bytes)) => bytes,
            Ok(None) => break,
            Err(_) => {
                let response = Response::failure(
                    "",
                    "limit_exceeded",
                    "Invalid, oversized, or stalled frame",
                    json!({}),
                );
                let _ = send(&writer, &response).await;
                // Framing cannot resume after a partial frame is rejected. Any
                // accepted mutation still completes durably, without appending
                // a later response to this closed outbound stream.
                let _ = writer.lock().await.shutdown().await;
                break;
            }
        };
        let admitted_at = std::time::Instant::now();
        while pending.try_join_next().is_some() {}
        let request = match decode_request(&bytes) {
            Ok(r) => r,
            Err(e) => {
                let id = serde_json::from_slice::<serde_json::Value>(&bytes)
                    .ok()
                    .and_then(|v| v["request_id"].as_str().map(str::to_owned))
                    .unwrap_or_default();
                let _ = send(
                    &writer,
                    &Response::failure(id, e.code, e.message, e.details),
                )
                .await;
                continue;
            }
        };
        let admission = match admitted.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                let _ = send(
                    &writer,
                    &Response::error(
                        &request,
                        "overloaded",
                        "Request/output admission limit reached",
                        json!({}),
                    ),
                )
                .await;
                continue;
            }
        };
        let pool = if request.operation == "repo.sync" {
            syncs.clone()
        } else if request.is_mutation() {
            heavy.clone()
        } else {
            reads.clone()
        };
        let permit = match pool.try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                let _ = send(
                    &writer,
                    &Response::error(&request, "overloaded", "Admission limit reached", json!({})),
                )
                .await;
                continue;
            }
        };
        let writer = writer.clone();
        let service = service.clone();
        pending.spawn(async move {
            let _admission = admission;
            let request_id = request.request_id.clone();
            let bytes = tokio::task::spawn_blocking(move || {
                // Encode and destroy the potentially large response on its
                // allocating worker, keeping socket runtime workers available.
                let response = service.execute_admitted(request, admitted_at);
                encode_frame(&response)
            })
            .await
            .unwrap_or_else(|_| {
                encode_frame(&Response::failure(
                    request_id,
                    "io_error",
                    "Request executor failed",
                    json!({}),
                ))
            });
            // Free computation capacity before exposing response bytes. Output
            // remains globally bounded by the separate admission permit.
            drop(permit);
            if let Ok(bytes) = bytes {
                let _ =
                    send_bytes_with_timeout(&writer, &bytes, std::time::Duration::from_secs(10))
                        .await;
            }
        });
    }
    while pending.join_next().await.is_some() {}
}

async fn send(
    writer: &tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>,
    response: &Response,
) -> std::io::Result<()> {
    send_with_timeout(writer, response, std::time::Duration::from_secs(10)).await
}
async fn send_with_timeout(
    writer: &tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>,
    response: &Response,
    deadline: std::time::Duration,
) -> std::io::Result<()> {
    let bytes = encode_frame(response)?;
    send_bytes_with_timeout(writer, &bytes, deadline).await
}
fn encode_frame(response: &Response) -> std::io::Result<Vec<u8>> {
    let (mut bytes, _) = encode_response(response)?;
    bytes.push(b'\n');
    Ok(bytes)
}
async fn send_bytes_with_timeout(
    writer: &tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>,
    bytes: &[u8],
    deadline: std::time::Duration,
) -> std::io::Result<()> {
    let mut writer = writer.lock().await;
    let result = tokio::time::timeout(deadline, writer.write_all(bytes))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "Slow response consumer"))
        .and_then(|result| result);
    if result.is_err() {
        // A partially written NDJSON frame cannot be repaired by appending the
        // next response. Close this outbound stream before releasing its lock.
        let _ = writer.shutdown().await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn assert_pending<F: std::future::Future>(mut future: std::pin::Pin<&mut F>) {
        std::future::poll_fn(|context| {
            assert!(
                future.as_mut().poll(context).is_pending(),
                "Frame completed unexpectedly"
            );
            std::task::Poll::Ready(())
        })
        .await;
    }
    #[tokio::test(start_paused = true)]
    async fn silent_connection_waits_for_long_request_then_expires_idle() {
        let (_peer, input) = tokio::io::duplex(1024);
        let mut reader = BufReader::new(input);
        let mut pending = tokio::task::JoinSet::new();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        pending.spawn(async move {
            let wait = tokio::time::sleep(std::time::Duration::from_secs(90));
            let _ = started_tx.send(());
            wait.await;
            let _ = done_tx.send(());
        });
        started_rx.await.unwrap();
        let reading = connection_frame(
            &mut reader,
            1024,
            &mut pending,
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(30),
        );
        tokio::pin!(reading);
        assert_pending(reading.as_mut()).await;
        tokio::time::advance(std::time::Duration::from_secs(31)).await;
        assert_pending(reading.as_mut()).await;
        tokio::time::advance(std::time::Duration::from_secs(59)).await;
        done_rx.await.unwrap();
        assert_pending(reading.as_mut()).await;
        tokio::time::advance(std::time::Duration::from_secs(29)).await;
        assert_pending(reading.as_mut()).await;
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        assert!(reading.await.unwrap().is_none());
    }
    #[tokio::test(start_paused = true)]
    async fn partial_frame_still_expires_while_long_request_is_pending() {
        let (mut peer, input) = tokio::io::duplex(1024);
        let mut reader = BufReader::new(input);
        let mut pending = tokio::task::JoinSet::new();
        pending.spawn(async {
            std::future::pending::<()>().await;
        });
        peer.write_all(b"{\"request_id\":").await.unwrap();
        let reading = connection_frame(
            &mut reader,
            1024,
            &mut pending,
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(30),
        );
        tokio::pin!(reading);
        assert_pending(reading.as_mut()).await;
        tokio::time::advance(std::time::Duration::from_secs(29)).await;
        assert_pending(reading.as_mut()).await;
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        assert_eq!(
            reading.await.unwrap_err().kind(),
            std::io::ErrorKind::TimedOut
        );
    }
    #[tokio::test(start_paused = true)]
    async fn pending_completion_mid_frame_preserves_every_consumed_byte() {
        let (mut peer, input) = tokio::io::duplex(1024);
        let mut reader = BufReader::new(input);
        let mut pending = tokio::task::JoinSet::new();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        pending.spawn(async move {
            finish_rx.await.unwrap();
            let _ = done_tx.send(());
        });
        peer.write_all(b"{\"request_id\":").await.unwrap();
        let reading = connection_frame(
            &mut reader,
            1024,
            &mut pending,
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(30),
        );
        tokio::pin!(reading);
        assert_pending(reading.as_mut()).await;
        tokio::time::advance(std::time::Duration::from_secs(10)).await;
        finish_tx.send(()).unwrap();
        done_rx.await.unwrap();
        assert_pending(reading.as_mut()).await;
        peer.write_all(b"\"kept\"}\n").await.unwrap();
        assert_eq!(
            reading.await.unwrap().unwrap(),
            b"{\"request_id\":\"kept\"}"
        );
    }
    #[tokio::test]
    async fn bounds_before_allocation() {
        let mut reader = BufReader::new(&b"123456789\n"[..]);
        assert!(frame(&mut reader, 4).await.is_err());
    }
    #[tokio::test]
    async fn partial_response_timeout_closes_output_before_next_response() {
        use tokio::io::AsyncReadExt;
        let (server, mut peer) = tokio::net::UnixStream::pair().unwrap();
        use std::os::fd::AsRawFd;
        let send_buffer = 4096i32;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    server.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&send_buffer as *const i32).cast(),
                    std::mem::size_of_val(&send_buffer) as libc::socklen_t,
                )
            },
            0
        );
        let (_, writer) = server.into_split();
        let writer = tokio::sync::Mutex::new(writer);
        let response = Response {
            protocol_version: 1,
            request_id: "large".into(),
            ok: true,
            result: Some(json!({"data":"x".repeat(2*1024*1024)})),
            error: None,
            operation_id: None,
            operation_state: None,
            resource_status: None,
        };
        assert!(
            send_with_timeout(&writer, &response, std::time::Duration::from_millis(1))
                .await
                .is_err()
        );
        let next = Response::failure("must-not-append", "overloaded", "second frame", json!({}));
        assert!(
            send_with_timeout(&writer, &next, std::time::Duration::from_millis(10))
                .await
                .is_err()
        );
        let mut bytes = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            peer.read_to_end(&mut bytes),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!bytes.is_empty());
        assert!(!bytes.contains(&b'\n'));
        assert!(
            !bytes
                .windows("must-not-append".len())
                .any(|w| w == b"must-not-append")
        );
    }
    #[tokio::test]
    async fn multiple_frames() {
        let mut reader = BufReader::new(&b"{}\n[]\n"[..]);
        assert_eq!(frame(&mut reader, 8).await.unwrap().unwrap(), b"{}");
        assert_eq!(frame(&mut reader, 8).await.unwrap().unwrap(), b"[]");
        assert!(frame(&mut reader, 8).await.unwrap().is_none());
    }
}

// Called before any threads exist. The lock's open-file description is inherited
// by the engine, while this parent retains it until every descendant is reaped.
fn guard(
    state: aq_git_core::state::StateRoot,
) -> Result<aq_git_core::state::StateRoot, Box<dyn std::error::Error>> {
    let incarnation = uuid::Uuid::new_v4().to_string();
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    state.mark_active(&incarnation)?;
    let engine = unsafe { libc::fork() };
    if engine < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if engine == 0 {
        return Ok(state);
    }
    GUARDED_ENGINE.store(engine, std::sync::atomic::Ordering::SeqCst);
    unsafe {
        libc::signal(
            libc::SIGINT,
            forward_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            forward_signal as *const () as libc::sighandler_t,
        );
    }
    // Leave the engine PID reserved as a zombie until signal forwarding is
    // disabled; a recycled PID must never receive a forwarded runtime signal.
    loop {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                engine as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result == 0 {
            break;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    GUARDED_ENGINE.store(0, std::sync::atomic::Ordering::SeqCst);
    let mut status = 0;
    if unsafe { libc::waitpid(engine, &mut status, 0) } != engine {
        return Err(std::io::Error::last_os_error().into());
    }
    let term_deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
    let cleanup_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let children =
            std::fs::read_to_string(format!("/proc/self/task/{}/children", std::process::id()))?;
        for pid in children
            .split_whitespace()
            .filter_map(|p| p.parse::<i32>().ok())
        {
            unsafe {
                libc::kill(
                    pid,
                    if std::time::Instant::now() < term_deadline {
                        libc::SIGTERM
                    } else {
                        libc::SIGKILL
                    },
                );
            }
        }
        let mut child_status = 0;
        let result = unsafe { libc::waitpid(-1, &mut child_status, libc::WNOHANG) };
        if result < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) {
            break;
        }
        if std::time::Instant::now() > cleanup_deadline {
            return Err("recovery_blocked: descendant cleanup could not be established".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    state.mark_clean(&incarnation)?;
    let code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        128 + libc::WTERMSIG(status)
    };
    std::process::exit(code);
}

static GUARDED_ENGINE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
extern "C" fn forward_signal(signal: libc::c_int) {
    let pid = GUARDED_ENGINE.load(std::sync::atomic::Ordering::Relaxed);
    if pid > 0 {
        unsafe {
            libc::kill(pid, signal);
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FenceClearance {
    expected_incarnation: String,
    reason: String,
    evidence: serde_json::Value,
    old_environment_stopped: bool,
}
fn clear_fence(args: Args) -> Result<i32, Box<dyn std::error::Error>> {
    if args.socket.is_some() {
        return Err("recovery clear-fence requires direct --state-dir".into());
    }
    let bytes = match args.input {
        Some(path) if path.as_os_str() != "-" => read_file(path)?,
        _ => read_stdin()?,
    };
    let request = decode_request(&bytes).map_err(|e| aq_git_core::Error::new(e.code, e.message))?;
    if request.operation != "recovery.clear-fence"
        || request.repository_id.is_some()
        || request.expected_binding_generation.is_some()
    {
        return Err("Invalid root-level fence clearance envelope".into());
    }
    let params: FenceClearance = serde_json::from_value(request.params.clone())?;
    aq_git_protocol::validate_id(&params.expected_incarnation)
        .map_err(|e| aq_git_core::Error::new(e.code, e.message))?;
    if !params.old_environment_stopped
        || params.reason.trim().is_empty()
        || !(params.evidence.is_object()
            && params.evidence.as_object().is_some_and(|o| !o.is_empty())
            || params
                .evidence
                .as_str()
                .is_some_and(|s| !s.trim().is_empty()))
    {
        return Err(
            "Clearance requires old_environment_stopped=true, a reason, and cleanup evidence"
                .into(),
        );
    }
    let root = aq_git_core::state::StateRoot::open_for_recovery(
        args.state_dir.ok_or("Choose --state-dir")?,
    )?;
    let id = request.operation_id.as_deref().unwrap();
    let prior = root.operation(id)?;
    let record = root.begin_operation(
        id,
        &json!({"operation":request.operation,"params":request.params}),
    )?;
    if matches!(record.status, aq_git_core::state::OperationStatus::Complete) {
        let mut response = Response::success(&request, record.result.unwrap());
        response.operation_state = Some("complete".into());
        print_response(&response, args.json, "recovery.clear-fence")?;
        return Ok(0);
    }
    let current = root.active_incarnation()?;
    let marker: Option<serde_json::Value> = root.read_json("incarnation.json")?;
    let resumed_clean = prior.is_some()
        && current.is_none()
        && marker.as_ref().is_some_and(|m| {
            m["id"].as_str() == Some(params.expected_incarnation.as_str()) && m["active"] == false
        });
    if current.as_deref() != Some(&params.expected_incarnation) && !resumed_clean {
        return Err(aq_git_core::Error::new(
            "incarnation_conflict",
            "Expected active incarnation does not match",
        )
        .into());
    }
    if !resumed_clean {
        root.mark_clean(&params.expected_incarnation)?;
    }
    let result = json!({"cleared_incarnation":params.expected_incarnation,"local_cleanup_attested":true,"remote_effects_resolved":false});
    root.complete_operation(id, record.generation, result.clone())?;
    let mut response = Response::success(&request, result);
    response.operation_state = Some("complete".into());
    print_response(&response, args.json, "recovery.clear-fence")?;
    Ok(0)
}

struct BoundedOutput(Vec<u8>);
impl std::io::Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > MAX_RESPONSE_BYTES {
            return Err(std::io::Error::other("Response limit exceeded"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn encode_response(response: &Response) -> std::io::Result<(Vec<u8>, bool)> {
    let mut output = BoundedOutput(Vec::new());
    if serde_json::to_writer(&mut output, response).is_ok() {
        return Ok((output.0, response.ok));
    }
    let response = Response::failure(
        &response.request_id,
        "limit_exceeded",
        "Response exceeds frame limit",
        json!({}),
    );
    Ok((serde_json::to_vec(&response)?, false))
}

fn exit_code(response: &Response) -> i32 {
    if !response.ok && response.error.is_none() {
        return 2;
    }
    let Some(error) = &response.error else {
        return if response
            .result
            .as_ref()
            .is_some_and(|r| r["outcome"] == "rejected")
        {
            3
        } else {
            0
        };
    };
    match error.code.as_str() {
        "invalid_response"
        | "invalid_request"
        | "invalid_input"
        | "invalid_id"
        | "invalid_path"
        | "invalid_artifact"
        | "artifact_invalid"
        | "unsupported_protocol"
        | "unsupported_feature"
        | "state_incompatible"
        | "unsupported_state_format" => 2,
        "operation_indeterminate" => 5,
        "overloaded" | "limit_exceeded" | "state_busy" | "deadline_exceeded"
        | "object_unavailable" | "remote_unavailable" => 4,
        "repository_not_found"
        | "workspace_not_found"
        | "candidate_not_found"
        | "operation_not_found"
        | "retention_not_found"
        | "binding_conflict"
        | "idempotency_conflict"
        | "generation_conflict"
        | "workspace_changed"
        | "workspace_dirty"
        | "target_moved"
        | "conflicted"
        | "observation_stale"
        | "prerequisite_missing"
        | "retention_in_use"
        | "binding_in_use"
        | "retention_released"
        | "recovery_blocked"
        | "incarnation_conflict"
        | "operation_terminal"
        | "retention_conflict" => 3,
        _ => 1,
    }
}
fn print_response(response: &Response, machine: bool, operation: &str) -> std::io::Result<i32> {
    use std::io::Write;
    let (bytes, ok) = encode_response(response)?;
    let effective = if ok == response.ok {
        response.clone()
    } else {
        serde_json::from_slice(&bytes)?
    };
    let mut stdout = std::io::stdout().lock();
    if machine {
        stdout.write_all(&bytes)?;
        stdout.write_all(b"\n")?;
        return Ok(exit_code(&effective));
    }
    if let Some(error) = &effective.error {
        writeln!(stdout, "{}: {} ({})", operation, error.message, error.code)?;
        if let Some(id) = &effective.operation_id {
            writeln!(stdout, "operation_id: {id}")?;
        }
        let next = match error.code.as_str() {
            "operation_indeterminate" => {
                "operation show, then operation reconcile with a new operation ID"
            }
            "state_busy" => "use --socket with the running service",
            "recovery_blocked" => "doctor; stop the old environment; audited recovery clear-fence",
            "repository_not_found"
            | "workspace_not_found"
            | "candidate_not_found"
            | "operation_not_found"
            | "retention_not_found"
            | "binding_conflict" => "repo show; refresh the expected binding generation",
            "observation_stale" => "repo sync",
            "object_unavailable" | "prerequisite_missing" => "repo sync or artifact import",
            "workspace_changed" | "workspace_dirty" => "workspace inspect",
            "retention_in_use" | "retention_released" => "retention inspect",
            _ => "inspect capabilities and correct the request",
        };
        writeln!(stdout, "next: {next}")?;
        if error.details != json!({}) && !error.details.is_null() {
            writeln!(stdout, "details: {}", error.details)?;
        }
    } else {
        writeln!(
            stdout,
            "{operation}: {}",
            effective
                .result
                .as_ref()
                .and_then(|r| r["outcome"].as_str().or_else(|| r["state"].as_str()))
                .unwrap_or("ok")
        )?;
        if let Some(result) = &effective.result {
            if let Some(object) = result.as_object() {
                for (key, value) in object {
                    if let Some(string) = value.as_str() {
                        writeln!(stdout, "{key}: {string}")?;
                    } else {
                        writeln!(stdout, "{key}: {value}")?;
                    }
                }
            } else {
                writeln!(stdout, "{result}")?;
            }
        }
        if let Some(id) = &effective.operation_id {
            writeln!(stdout, "operation_id: {id}")?;
        }
    }
    Ok(exit_code(&effective))
}
