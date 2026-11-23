use super::socket::{Admission, serve_connection};
use super::*;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};

pub(super) fn run_server(
    args: Args,
    state: Option<StateRoot>,
    runtime: tokio::runtime::Runtime,
) -> CliResult<i32> {
    args.state_dir
        .as_ref()
        .ok_or("serve requires --state-dir")?;
    let socket = args.listen.as_ref().ok_or("serve requires --listen PATH")?;
    let service = direct_service(
        state.unwrap(),
        args.git_executable,
        args.credential_profiles,
    )?;
    let service = Arc::new(service);
    let git_http = args
        .git_http_socket
        .as_deref()
        .map(|path| start_git_http(path, service.clone()))
        .transpose()?;
    let sweeper = start_temp_pack_sweeper(service.clone())?;
    let served = runtime.block_on(serve(socket, service));
    sweeper.stop();
    if let Some((server, path, identity)) = git_http {
        stop_git_http(server, &path, identity);
    }
    served?;
    Ok(0)
}
/// Removes stale pool temporary packs on start and then every configured
/// interval, reporting each sweep as one JSON line in the service log (stderr).
struct TempPackSweeper {
    stop: std::sync::mpsc::Sender<()>,
    thread: std::thread::JoinHandle<()>,
}
impl TempPackSweeper {
    fn stop(self) {
        drop(self.stop);
        let _ = self.thread.join();
    }
}
fn start_temp_pack_sweeper(
    service: Arc<RepositoryService>,
) -> Result<TempPackSweeper, Box<dyn std::error::Error>> {
    // Refuse an invalid configuration at startup rather than sweeping with a guess.
    let mut config = service.temp_pack_sweep_config()?;
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("temp-pack-sweep".into())
        .spawn(move || {
            let mut trigger = "start";
            loop {
                match service.sweep_stale_temp_packs() {
                    Ok(report) => eprintln!(
                        "{}",
                        json!({"event":"temp_pack_sweep","trigger":trigger,"max_age_secs":config.max_age_secs,
                            "pools_scanned":report.pools_scanned,"files_removed":report.files_removed,
                            "bytes_removed":report.bytes_removed,"files_kept_open":report.files_kept_open,
                            "staging_directories_removed":report.staging_directories_removed,"errors":report.errors})
                    ),
                    Err(error) => eprintln!(
                        "{}",
                        json!({"event":"temp_pack_sweep","trigger":trigger,"error_code":error.code,"message":error.message})
                    ),
                }
                trigger = "periodic";
                match stopped.recv_timeout(config.interval()) {
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    _ => return,
                }
                if let Ok(current) = service.temp_pack_sweep_config() {
                    config = current;
                }
            }
        })?;
    Ok(TempPackSweeper { stop, thread })
}
async fn serve(
    path: &std::path::Path,
    service: Arc<RepositoryService>,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = absolute_path(path)?;
    let path = path.as_path();
    remove_stale_socket(path, &service).await?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let bound_metadata = std::fs::symlink_metadata(path)?;
    service.root.write_json("service-socket.json",&json!({"active":true,"path":path,"device":bound_metadata.dev(),"inode":bound_metadata.ino()}))?;
    let connections = Arc::new(tokio::sync::Semaphore::new(64));
    let admission = Admission::new();
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
                let service=service.clone();
                let admission=admission.clone();
                tasks.spawn(async move {
                    let _connection=connection;
                    serve_connection(stream,service,admission).await;
                });
            }
        }
    }
    drop(listener);
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    let current = std::fs::symlink_metadata(path)?;
    if identity(&current) != identity(&bound_metadata) {
        return Err("Bound socket path was replaced; refusing to remove replacement".into());
    }
    std::fs::remove_file(path)?;
    service.root.write_json("service-socket.json",&json!({"active":false,"path":path,"device":bound_metadata.dev(),"inode":bound_metadata.ino()}))?;
    Ok(())
}
fn absolute_path(path: &std::path::Path) -> std::io::Result<PathBuf> {
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    })
}
async fn remove_stale_socket(path: &std::path::Path, service: &RepositoryService) -> CliResult<()> {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
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
    if !owner
        .as_ref()
        .is_some_and(|o| o["active"] == true && owns_socket(o, path, &metadata))
    {
        return Err("Stale socket is not recorded as owned by this state root".into());
    }
    let rechecked = std::fs::symlink_metadata(path)?;
    if identity(&rechecked) != identity(&metadata) {
        return Err("Socket changed during ownership verification".into());
    }
    std::fs::remove_file(path)?;
    Ok(())
}
/// The running endpoint, its socket path and the (device, inode) it bound.
type GitHttpHandle = (aq_git_core::git_http::GitHttpServer, PathBuf, (u64, u64));
/// Binds the Git smart-HTTP socket with the same ownership rules as the JSON
/// socket: only a stale socket this state root recorded may be replaced.
fn start_git_http(
    path: &std::path::Path,
    service: Arc<RepositoryService>,
) -> Result<GitHttpHandle, Box<dyn std::error::Error>> {
    let path = absolute_path(path)?;
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        if !metadata.file_type().is_socket() {
            return Err("Refusing to replace a non-socket path".into());
        }
        if !matches!(
            std::os::unix::net::UnixStream::connect(&path),
            Err(ref e) if e.kind() == std::io::ErrorKind::ConnectionRefused
        ) {
            return Err("Refusing to replace a live or unverified socket".into());
        }
        let owner: Option<serde_json::Value> = service.root.read_json("git-http-socket.json")?;
        if !owner
            .as_ref()
            .is_some_and(|o| owns_socket(o, &path, &metadata))
        {
            return Err("Stale Git HTTP socket is not recorded as owned by this state root".into());
        }
        std::fs::remove_file(&path)?;
    }
    let listener = std::os::unix::net::UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let bound = std::fs::symlink_metadata(&path)?;
    let identity = identity(&bound);
    service.root.write_json(
        "git-http-socket.json",
        &json!({"path":path,"device":identity.0,"inode":identity.1}),
    )?;
    let server =
        aq_git_core::git_http::GitHttpServer::start(service, listener, std::env::current_exe()?)?;
    Ok((server, path, identity))
}
fn stop_git_http(
    server: aq_git_core::git_http::GitHttpServer,
    path: &std::path::Path,
    bound_identity: (u64, u64),
) {
    server.stop();
    // Remove only the socket this process bound, never a replacement.
    if std::fs::symlink_metadata(path).is_ok_and(|m| identity(&m) == bound_identity) {
        let _ = std::fs::remove_file(path);
    }
}

fn identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}
fn owns_socket(
    owner: &serde_json::Value,
    path: &std::path::Path,
    metadata: &std::fs::Metadata,
) -> bool {
    owner["path"].as_str() == path.to_str()
        && owner["device"].as_u64().zip(owner["inode"].as_u64()) == Some(identity(metadata))
}
