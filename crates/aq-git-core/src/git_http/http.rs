//! The small HTTP/1.1 subset aq-server's forwarder and a Git client use:
//! one request per connection, `Content-Length` or chunked bodies, optional gzip.
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub(crate) struct Head {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
}
impl Head {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.trim())
    }
}
#[derive(Debug)]
pub(crate) struct HttpError {
    pub status: u16,
    pub message: String,
}
impl HttpError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}
impl From<crate::Error> for HttpError {
    fn from(error: crate::Error) -> Self {
        let status = match error.code.as_str() {
            "invalid_request" | "invalid_grant" | "invalid_path" => 400,
            "repository_not_found" | "state_not_found" => 404,
            "limit_exceeded" => 413,
            "overloaded" => 503,
            "deadline_exceeded" | "git_timeout" => 504,
            "binding_conflict"
            | "recovery_blocked"
            | "retention_conflict"
            | "idempotency_conflict"
            | "object_unavailable"
            | "prerequisite_missing"
            | "unsupported_feature" => 409,
            _ => 500,
        };
        Self::new(status, format!("{}: {}", error.code, error.message))
    }
}
impl From<io::Error> for HttpError {
    fn from(error: io::Error) -> Self {
        Self::new(500, format!("io_error: {error}"))
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Internal Server Error",
    }
}

/// Reads one CRLF/LF line of at most `limit` bytes (excluding the terminator).
fn read_line<R: BufRead>(reader: &mut R, limit: usize) -> Result<Option<String>, HttpError> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(HttpError::new(400, "truncated request"))
            };
        }
        let (used, done) = match available.iter().position(|b| *b == b'\n') {
            Some(i) => (i + 1, true),
            None => (available.len(), false),
        };
        if line.len() + used > limit + 2 {
            return Err(HttpError::new(413, "request line or header too large"));
        }
        line.extend_from_slice(&available[..used]);
        reader.consume(used);
        if done {
            break;
        }
    }
    line.pop();
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    String::from_utf8(line)
        .map(Some)
        .map_err(|_| HttpError::new(400, "request head is not UTF-8"))
}

pub(crate) fn read_head<R: BufRead>(reader: &mut R, max_bytes: usize) -> Result<Head, HttpError> {
    let mut budget = max_bytes;
    let line = read_line(reader, budget)?.ok_or_else(|| HttpError::new(400, "empty request"))?;
    budget = budget.saturating_sub(line.len() + 2);
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(HttpError::new(400, "malformed request line"));
    };
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(HttpError::new(400, "unsupported HTTP version"));
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut headers = Vec::new();
    loop {
        let line =
            read_line(reader, budget)?.ok_or_else(|| HttpError::new(400, "truncated head"))?;
        if line.is_empty() {
            break;
        }
        budget = budget.saturating_sub(line.len() + 2);
        let (name, value) = line
            .split_once(':')
            .filter(|(n, _)| !n.is_empty() && !n.contains(char::is_whitespace))
            .ok_or_else(|| HttpError::new(400, "malformed header"))?;
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    Ok(Head {
        method: method.into(),
        path: path.into(),
        query: query.into(),
        headers,
    })
}

struct Chunked<R> {
    inner: R,
    remaining: u64,
    done: bool,
}
impl<R: BufRead> Chunked<R> {
    fn line(&mut self) -> io::Result<String> {
        read_line(&mut self.inner, 4096)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.message))?
            .ok_or_else(|| io::ErrorKind::UnexpectedEof.into())
    }
}
impl<R: BufRead> Read for Chunked<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let line = self.line()?;
            let size = line.split(';').next().unwrap_or("").trim();
            self.remaining = u64::from_str_radix(size, 16)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
            if self.remaining == 0 {
                while !self.line()?.is_empty() {}
                self.done = true;
                return Ok(0);
            }
        }
        let want = buf
            .len()
            .min(self.remaining.min(usize::MAX as u64) as usize);
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.remaining -= n as u64;
        if self.remaining == 0 && !self.line()?.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad chunk end"));
        }
        Ok(n)
    }
}

/// Fails reads after `limit` bytes and records that it did.
pub(crate) struct Limited<R> {
    inner: R,
    left: u64,
    pub exceeded: Arc<AtomicBool>,
}
impl<R: Read> Read for Limited<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n as u64 > self.left {
            self.exceeded.store(true, Ordering::SeqCst);
            return Err(io::Error::other("body exceeds the byte limit"));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

/// The decoded request body, limited to `limit` decoded bytes. A declared
/// `Content-Length` above the limit fails up front with 413.
pub(crate) fn body<'a, R: BufRead + Send + 'a>(
    head: &Head,
    reader: R,
    limit: u64,
    exceeded: Arc<AtomicBool>,
) -> Result<Box<dyn Read + Send + 'a>, HttpError> {
    let raw: Box<dyn Read + Send + 'a> = match (
        head.header("transfer-encoding"),
        head.header("content-length"),
    ) {
        (Some(te), None) if te.eq_ignore_ascii_case("chunked") => Box::new(Chunked {
            inner: reader,
            remaining: 0,
            done: false,
        }),
        (None, Some(length)) => {
            let length: u64 = length
                .parse()
                .map_err(|_| HttpError::new(400, "bad Content-Length"))?;
            if head.header("content-encoding").is_none() && length > limit {
                return Err(HttpError::new(413, "request body exceeds the byte limit"));
            }
            Box::new(reader.take(length))
        }
        (None, None) => Box::new(io::empty()),
        _ => return Err(HttpError::new(400, "unsupported body framing")),
    };
    let decoded: Box<dyn Read + Send + 'a> = match head.header("content-encoding") {
        None | Some("identity") => raw,
        Some("gzip") | Some("x-gzip") => Box::new(flate2::read::MultiGzDecoder::new(raw)),
        Some(_) => return Err(HttpError::new(415, "unsupported Content-Encoding")),
    };
    Ok(Box::new(Limited {
        inner: decoded,
        left: limit,
        exceeded,
    }))
}

pub(crate) fn write_head<W: Write>(
    out: &mut W,
    status: u16,
    headers: &[(&str, String)],
) -> io::Result<()> {
    write!(out, "HTTP/1.1 {status} {}\r\n", reason(status))?;
    for (name, value) in headers {
        write!(out, "{name}: {value}\r\n")?;
    }
    write!(out, "Connection: close\r\n\r\n")
}
pub(crate) fn respond<W: Write>(
    out: &mut W,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> io::Result<()> {
    write_head(
        out,
        status,
        &[
            ("Content-Type", content_type.into()),
            ("Content-Length", body.len().to_string()),
            ("Cache-Control", "no-cache".into()),
        ],
    )?;
    out.write_all(body)?;
    out.flush()
}
pub(crate) fn respond_error<W: Write>(out: &mut W, error: &HttpError) -> io::Result<()> {
    respond(
        out,
        error.status,
        "text/plain; charset=utf-8",
        format!("aq-git: {}\n", error.message).as_bytes(),
    )
}
/// `Transfer-Encoding: chunked` response body.
pub(crate) struct ChunkedWriter<W: Write>(pub W);
impl<W: Write> ChunkedWriter<W> {
    pub fn chunk(&mut self, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        write!(self.0, "{:x}\r\n", data.len())?;
        self.0.write_all(data)?;
        self.0.write_all(b"\r\n")
    }
    pub fn finish(mut self) -> io::Result<()> {
        self.0.write_all(b"0\r\n\r\n")?;
        self.0.flush()
    }
}
pub(crate) fn buffered<R: Read>(inner: R) -> BufReader<R> {
    BufReader::with_capacity(64 * 1024, inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn decode(raw: &[u8]) -> io::Result<Vec<u8>> {
        let head = Head {
            method: "POST".into(),
            path: "/".into(),
            query: String::new(),
            headers: vec![("transfer-encoding".into(), "chunked".into())],
        };
        let mut out = Vec::new();
        body(&head, raw, 1000, Default::default())
            .unwrap_or_else(|_| panic!("framing"))
            .read_to_end(&mut out)?;
        Ok(out)
    }
    #[test]
    fn chunked_bodies_decode_and_malformed_ones_fail() {
        assert_eq!(
            decode(b"5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\nTrailer: x\r\n\r\n").unwrap(),
            b"hello world"
        );
        assert!(decode(b"zz\r\nhello\r\n0\r\n\r\n").is_err());
        assert!(decode(b"5\r\nhelloXX0\r\n\r\n").is_err());
        assert!(decode(b"5\r\nhel").is_err());
    }
    #[test]
    fn gzip_bodies_decode_within_the_decoded_limit() {
        use std::io::Write as _;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&[b'a'; 500]).unwrap();
        let compressed = encoder.finish().unwrap();
        let head = Head {
            method: "POST".into(),
            path: "/".into(),
            query: String::new(),
            headers: vec![
                ("content-length".into(), compressed.len().to_string()),
                ("content-encoding".into(), "gzip".into()),
            ],
        };
        let mut out = Vec::new();
        body(&head, &compressed[..], 1000, Default::default())
            .unwrap_or_else(|_| panic!("framing"))
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, [b'a'; 500]);
        let exceeded = Arc::new(AtomicBool::new(false));
        let mut limited = body(&head, &compressed[..], 100, exceeded.clone())
            .unwrap_or_else(|_| panic!("framing"));
        assert!(limited.read_to_end(&mut Vec::new()).is_err());
        assert!(exceeded.load(Ordering::SeqCst));
    }
    #[test]
    fn body_limit_is_enforced_for_declared_and_streamed_bodies() {
        let mut head = Head {
            method: "POST".into(),
            path: "/".into(),
            query: String::new(),
            headers: vec![("content-length".into(), "11".into())],
        };
        assert_eq!(
            body(&head, &b"hello world"[..], 10, Default::default())
                .err()
                .unwrap()
                .status,
            413
        );
        head.headers = vec![("transfer-encoding".into(), "chunked".into())];
        let exceeded = Arc::new(AtomicBool::new(false));
        let mut reader = body(
            &head,
            &b"b\r\nhello world\r\n0\r\n\r\n"[..],
            10,
            exceeded.clone(),
        )
        .unwrap_or_else(|_| panic!("framing"));
        assert!(reader.read_to_end(&mut Vec::new()).is_err());
        assert!(exceeded.load(Ordering::SeqCst));
    }
    #[test]
    fn head_parsing_lowercases_names_and_bounds_size() {
        let raw =
            b"GET /r/info/refs?service=git-upload-pack HTTP/1.1\r\nAQ-Base: x\r\nHost: h\r\n\r\n";
        let head = read_head(&mut &raw[..], 4096).unwrap();
        assert_eq!(head.path, "/r/info/refs");
        assert_eq!(head.query, "service=git-upload-pack");
        assert_eq!(head.header("aq-base"), Some("x"));
        let mut big = b"GET / HTTP/1.1\r\n".to_vec();
        big.extend(std::iter::repeat_n(b'a', 5000));
        assert_eq!(read_head(&mut &big[..], 1024).err().unwrap().status, 413);
        assert!(read_head(&mut &b"BAD\r\n\r\n"[..], 1024).is_err());
    }
}
