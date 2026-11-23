use super::response::encode_response;
use super::*;
async fn frame<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    max: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return frame_eof(&bytes);
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
            return Ok(Some(bytes));
        }
    }
}
fn frame_eof(bytes: &[u8]) -> std::io::Result<Option<Vec<u8>>> {
    if bytes.is_empty() {
        Ok(None)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "Incomplete NDJSON frame",
        ))
    }
}
pub(super) async fn client(
    path: PathBuf,
    request: &Request,
) -> Result<Response, Box<dyn std::error::Error>> {
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
    if request.is_mutation() {
        mark_uncertain_mutation(&mut response);
    }
    Ok(response)
}
fn mark_uncertain_mutation(response: &mut Response) {
    let Some(error) = &mut response.error else {
        return;
    };
    if matches!(
        error.code.as_str(),
        "invalid_response" | "unsupported_protocol"
    ) {
        if !error.details.is_object() {
            let server_details = error.details.take();
            error.details = json!({"server_details":server_details});
        }
        error.details["operation_outcome"] = json!("unknown");
        error.retry = aq_git_protocol::RetryAdvice::ReconcileFirst;
    }
}
// Waiting for another request is separate from assembling a request already
// started. A quiet client with bounded work outstanding is not a stalled frame.
async fn connection_frame<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    max: usize,
    pending: &mut tokio::task::JoinSet<()>,
    (idle_timeout, partial_timeout): (std::time::Duration, std::time::Duration),
) -> std::io::Result<Option<Vec<u8>>> {
    if !wait_for_frame(reader, pending, idle_timeout).await? {
        return Ok(None);
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
async fn wait_for_frame<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    pending: &mut tokio::task::JoinSet<()>,
    idle_timeout: std::time::Duration,
) -> std::io::Result<bool> {
    let first_byte = reader.fill_buf();
    tokio::pin!(first_byte);
    let idle = tokio::time::sleep(idle_timeout);
    tokio::pin!(idle);
    loop {
        tokio::select! {
            data=&mut first_byte=>{
                return Ok(!data?.is_empty());
            }
            _=pending.join_next(),if !pending.is_empty()=>{
                if pending.is_empty(){idle.as_mut().reset(tokio::time::Instant::now()+idle_timeout);}
            }
            _=&mut idle,if pending.is_empty()=>return Ok(false),
        }
    }
}
#[derive(Clone)]
pub(super) struct Admission {
    reads: Arc<tokio::sync::Semaphore>,
    heavy: Arc<tokio::sync::Semaphore>,
    syncs: Arc<tokio::sync::Semaphore>,
    admitted: Arc<tokio::sync::Semaphore>,
}
type Writer = tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>;
type Permit = tokio::sync::OwnedSemaphorePermit;
impl Admission {
    pub(super) fn new() -> Self {
        Self {
            reads: Arc::new(tokio::sync::Semaphore::new(32)),
            heavy: Arc::new(tokio::sync::Semaphore::new(4)),
            syncs: Arc::new(tokio::sync::Semaphore::new(32)),
            admitted: Arc::new(tokio::sync::Semaphore::new(64)),
        }
    }
    async fn permits(&self, request: &Request, writer: &Writer) -> Option<(Permit, Permit)> {
        let admission = acquire_permit(
            self.admitted.clone(),
            request,
            writer,
            "Request/output admission limit reached",
        )
        .await?;
        let pool = if request.operation == "repo.sync"
            || request.operation == aq_git_protocol::REPO_RETAIN_REF
        {
            self.syncs.clone()
        } else if request.is_mutation() {
            self.heavy.clone()
        } else {
            self.reads.clone()
        };
        let permit = acquire_permit(pool, request, writer, "Admission limit reached").await?;
        Some((admission, permit))
    }
}

async fn acquire_permit(
    pool: Arc<tokio::sync::Semaphore>,
    request: &Request,
    writer: &Writer,
    message: &str,
) -> Option<Permit> {
    match pool.try_acquire_owned() {
        Ok(p) => Some(p),
        Err(_) => {
            let _ = send(
                writer,
                &Response::error(request, "overloaded", message, json!({})),
            )
            .await;
            None
        }
    }
}
pub(super) async fn serve_connection(
    stream: tokio::net::UnixStream,
    service: Arc<RepositoryService>,
    capacity: Admission,
) {
    let (r, w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let writer = Arc::new(tokio::sync::Mutex::new(w));
    let mut pending = tokio::task::JoinSet::new();
    loop {
        let Some(bytes) = next_frame(&mut reader, &mut pending, &writer).await else {
            break;
        };
        let admitted_at = std::time::Instant::now();
        while pending.try_join_next().is_some() {}
        let request = match decode_request(&bytes) {
            Ok(r) => r,
            Err(e) => {
                let _ = send(&writer, &decode_failure(&bytes, e)).await;
                continue;
            }
        };
        let Some((admission, permit)) = capacity.permits(&request, &writer).await else {
            continue;
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
async fn next_frame<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    pending: &mut tokio::task::JoinSet<()>,
    writer: &Writer,
) -> Option<Vec<u8>> {
    match connection_frame(
        reader,
        MAX_REQUEST_BYTES,
        pending,
        (
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(30),
        ),
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(_) => {
            let response = Response::failure(
                "",
                "limit_exceeded",
                "Invalid, oversized, or stalled frame",
                json!({}),
            );
            let _ = send(writer, &response).await;
            // Framing cannot resume after a partial frame is rejected. Any
            // accepted mutation still completes durably, without appending
            // a later response to this closed outbound stream.
            let _ = writer.lock().await.shutdown().await;
            None
        }
    }
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
            (
                std::time::Duration::from_secs(30),
                std::time::Duration::from_secs(30),
            ),
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
            (
                std::time::Duration::from_secs(30),
                std::time::Duration::from_secs(30),
            ),
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
            (
                std::time::Duration::from_secs(30),
                std::time::Duration::from_secs(30),
            ),
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
