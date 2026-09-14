//! Shared deterministic temp directory helpers for unit tests.

use std::cell::Cell;
use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Owns a uniquely named directory under the system temp folder.
///
/// The directory is removed when the fixture is dropped. Cleanup failures are
/// reported to the test diagnostic stream instead of being discarded.
#[derive(Debug)]
pub struct TestTempDir {
    path: PathBuf,
    cleaned: Cell<bool>,
}

impl TestTempDir {
    /// Create a uniquely named directory under the system temp folder.
    pub fn new(label: &str) -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!("harmonium-test-{label}-{nanos}-{id}"));

        fs::create_dir_all(&path).expect("temp dir creation");
        Self {
            path,
            cleaned: Cell::new(false),
        }
    }

    /// Return the owned directory path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Remove the directory immediately and return any unexpected failure.
    pub fn cleanup(&self) -> io::Result<()> {
        if self.cleaned.get() {
            return Ok(());
        }

        match fs::remove_dir_all(&self.path) {
            Ok(()) => {
                self.cleaned.set(true);
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.cleaned.set(true);
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}

impl AsRef<Path> for TestTempDir {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}

impl Deref for TestTempDir {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.path()
    }
}

impl Drop for TestTempDir {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            eprintln!(
                "temporary test fixture cleanup failed for {}: {error}",
                self.path.display()
            );
        }
    }
}

/// Create a uniquely named, owned directory under the system temp folder.
pub fn unique_temp_dir(label: &str) -> TestTempDir {
    TestTempDir::new(label)
}

const SCRIPTED_HTTP_IO_TIMEOUT: Duration = Duration::from_millis(100);
const SCRIPTED_HTTP_POLL_INTERVAL: Duration = Duration::from_millis(5);
const SCRIPTED_HTTP_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(250);
const SCRIPTED_HTTP_MAX_REQUEST_BYTES: usize = 16 * 1024;
const SCRIPTED_HTTP_WRITE_CHUNK_BYTES: usize = 16 * 1024;

/// A deterministic response consumed by [`ScriptedHttpServer`] in request order.
#[derive(Debug, Clone)]
pub struct ScriptedHttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: ScriptedHttpBody,
    header_delay: Duration,
}

#[derive(Debug, Clone)]
enum ScriptedHttpBody {
    Fixed(Vec<u8>),
    Chunked {
        chunks: Vec<Vec<u8>>,
        delay: Duration,
    },
    CloseAfter {
        body: Vec<u8>,
        declared_length: usize,
    },
    Raw(Vec<u8>),
}

impl ScriptedHttpResponse {
    /// Build a normal response with a fixed body and an inferred content length.
    pub fn fixed(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: ScriptedHttpBody::Fixed(body.into()),
            header_delay: Duration::ZERO,
        }
    }

    /// Build a response whose body is sent as HTTP chunks with a delay between chunks.
    pub fn chunked(
        status: u16,
        chunks: impl IntoIterator<Item = impl Into<Vec<u8>>>,
        delay: Duration,
    ) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: ScriptedHttpBody::Chunked {
                chunks: chunks.into_iter().map(Into::into).collect(),
                delay,
            },
            header_delay: Duration::ZERO,
        }
    }

    /// Build a response that declares `declared_length` but closes after `body`.
    pub fn partial(status: u16, body: impl Into<Vec<u8>>, declared_length: usize) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: ScriptedHttpBody::CloseAfter {
                body: body.into(),
                declared_length,
            },
            header_delay: Duration::ZERO,
        }
    }

    /// Build a response that closes while transferring a body.
    pub fn close_during_body(status: u16, body: impl Into<Vec<u8>>) -> Self {
        let body = body.into();
        Self::partial(status, body.clone(), body.len().saturating_add(1))
    }

    /// Write an intentionally malformed HTTP response verbatim.
    pub fn malformed(raw: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 0,
            headers: Vec::new(),
            body: ScriptedHttpBody::Raw(raw.into()),
            header_delay: Duration::ZERO,
        }
    }

    /// Add a response header without changing the scripted body behavior.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Delay response headers so cancellation during a blocking request is
    /// deterministic in provider boundary tests.
    pub fn delay_headers(mut self, delay: Duration) -> Self {
        self.header_delay = delay;
        self
    }
}

/// Loopback-only HTTP server for deterministic provider boundary tests.
///
/// The server consumes scripted responses in request order. It records bounded
/// request text for assertions and owns its listener thread, so dropping it
/// requests shutdown and waits only for a bounded period before detaching.
pub struct ScriptedHttpServer {
    address: SocketAddr,
    responses: Arc<Mutex<VecDeque<ScriptedHttpResponse>>>,
    requests: Arc<Mutex<Vec<String>>>,
    shutdown: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
}

impl ScriptedHttpServer {
    /// Bind an ephemeral listener on IPv4 loopback and load the response script.
    pub fn new(responses: impl IntoIterator<Item = ScriptedHttpResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind scripted HTTP server");
        listener
            .set_nonblocking(true)
            .expect("set scripted HTTP server nonblocking");
        let address = listener.local_addr().expect("scripted HTTP server address");
        let responses = Arc::new(Mutex::new(responses.into_iter().collect::<VecDeque<_>>()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let responses_for_thread = Arc::clone(&responses);
        let requests_for_thread = Arc::clone(&requests);
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_for_thread = Arc::clone(&shutdown);
        let listener_thread = thread::spawn(move || {
            run_scripted_http_server(
                listener,
                responses_for_thread,
                requests_for_thread,
                shutdown_for_thread,
            );
        });

        Self {
            address,
            responses,
            requests,
            shutdown: Some((shutdown, listener_thread)),
        }
    }

    /// Append a response after binding when its body needs the assigned port.
    pub fn push_response(&self, response: ScriptedHttpResponse) {
        self.responses
            .lock()
            .expect("scripted response lock")
            .push_back(response);
    }

    /// Return the server root URL.
    pub fn url(&self) -> String {
        format!("http://{}/", self.address)
    }

    /// Return a URL below the server root.
    pub fn endpoint(&self, path: &str) -> String {
        let path = path.trim_start_matches('/');
        format!("http://{}/{}", self.address, path)
    }

    /// Return the bounded raw requests accepted so far.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("scripted request lock").clone()
    }
}

impl Drop for ScriptedHttpServer {
    fn drop(&mut self) {
        if let Some((shutdown, listener_thread)) = self.shutdown.take() {
            shutdown.store(true, Ordering::Release);
            let deadline = std::time::Instant::now() + SCRIPTED_HTTP_SHUTDOWN_TIMEOUT;
            while !listener_thread.is_finished() && std::time::Instant::now() < deadline {
                thread::sleep(SCRIPTED_HTTP_POLL_INTERVAL);
            }
            if listener_thread.is_finished() {
                let _ = listener_thread.join();
            }
        }
    }
}

fn run_scripted_http_server(
    listener: TcpListener,
    responses: Arc<Mutex<VecDeque<ScriptedHttpResponse>>>,
    requests: Arc<Mutex<Vec<String>>>,
    shutdown: Arc<AtomicBool>,
) {
    loop {
        if shutdown.load(Ordering::Acquire) {
            return;
        }

        match listener.accept() {
            Ok((stream, _)) => {
                let request = read_request(stream.try_clone().expect("clone scripted stream"));
                requests
                    .lock()
                    .expect("scripted request lock")
                    .push(request);
                let response = responses
                    .lock()
                    .expect("scripted response lock")
                    .pop_front()
                    .unwrap_or_else(|| ScriptedHttpResponse::fixed(500, b"script exhausted"));
                write_scripted_response(stream, response, &shutdown);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(SCRIPTED_HTTP_POLL_INTERVAL);
            }
            Err(_) => return,
        }
    }
}

fn read_request(mut stream: TcpStream) -> String {
    let _ = stream.set_read_timeout(Some(SCRIPTED_HTTP_IO_TIMEOUT));
    let mut bytes = Vec::with_capacity(1024);
    let mut buffer = [0u8; 1024];
    while bytes.len() < SCRIPTED_HTTP_MAX_REQUEST_BYTES {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                let remaining = SCRIPTED_HTTP_MAX_REQUEST_BYTES - bytes.len();
                bytes.extend_from_slice(&buffer[..read.min(remaining)]);
                if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn write_scripted_response(
    mut stream: TcpStream,
    response: ScriptedHttpResponse,
    shutdown: &AtomicBool,
) {
    let _ = stream.set_write_timeout(Some(SCRIPTED_HTTP_IO_TIMEOUT));
    if wait_for_delay(shutdown, response.header_delay) {
        return;
    }
    match response.body {
        ScriptedHttpBody::Raw(raw) => {
            let _ = write_body_bytes(&mut stream, &raw, shutdown);
        }
        ScriptedHttpBody::Fixed(body) => {
            let mut headers = response.headers;
            add_default_header(&mut headers, "Content-Length", body.len().to_string());
            add_default_header(&mut headers, "Connection", "close");
            if write_headers(&mut stream, response.status, &headers).is_ok() {
                let _ = write_body_bytes(&mut stream, &body, shutdown);
            }
        }
        ScriptedHttpBody::Chunked { chunks, delay } => {
            let mut headers = response.headers;
            add_default_header(&mut headers, "Transfer-Encoding", "chunked");
            add_default_header(&mut headers, "Connection", "close");
            if write_headers(&mut stream, response.status, &headers).is_err() {
                return;
            }
            for (index, chunk) in chunks.iter().enumerate() {
                if shutdown.load(Ordering::Acquire) {
                    return;
                }
                let prefix = format!("{:X}\r\n", chunk.len());
                if stream.write_all(prefix.as_bytes()).is_err()
                    || write_body_bytes(&mut stream, chunk, shutdown).is_err()
                    || stream.write_all(b"\r\n").is_err()
                {
                    return;
                }
                let _ = stream.flush();
                if index + 1 < chunks.len() && wait_for_delay(shutdown, delay) {
                    return;
                }
            }
            let _ = stream.write_all(b"0\r\n\r\n");
        }
        ScriptedHttpBody::CloseAfter {
            body,
            declared_length,
        } => {
            let mut headers = response.headers;
            add_default_header(&mut headers, "Content-Length", declared_length.to_string());
            add_default_header(&mut headers, "Connection", "close");
            if write_headers(&mut stream, response.status, &headers).is_ok() {
                let _ = write_body_bytes(&mut stream, &body, shutdown);
            }
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

fn write_headers(
    stream: &mut TcpStream,
    status: u16,
    headers: &[(String, String)],
) -> io::Result<()> {
    let mut response = format!("HTTP/1.1 {status} {}\r\n", status_reason(status));
    for (name, value) in headers {
        response.push_str(name);
        response.push_str(": ");
        response.push_str(value);
        response.push_str("\r\n");
    }
    response.push_str("\r\n");
    stream.write_all(response.as_bytes())
}

fn write_body_bytes(stream: &mut TcpStream, body: &[u8], shutdown: &AtomicBool) -> io::Result<()> {
    for chunk in body.chunks(SCRIPTED_HTTP_WRITE_CHUNK_BYTES) {
        if shutdown.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "scripted server shutdown requested",
            ));
        }
        stream.write_all(chunk)?;
    }
    Ok(())
}

fn wait_for_delay(shutdown: &AtomicBool, delay: Duration) -> bool {
    let deadline = std::time::Instant::now() + delay;
    loop {
        if shutdown.load(Ordering::Acquire) {
            return true;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        thread::sleep(remaining.min(SCRIPTED_HTTP_POLL_INTERVAL));
    }
}

fn add_default_header(headers: &mut Vec<(String, String)>, name: &str, value: impl Into<String>) {
    if !headers
        .iter()
        .any(|(header, _)| header.eq_ignore_ascii_case(name))
    {
        headers.push((name.to_string(), value.into()));
    }
}

fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Scripted",
    }
}

/// Minimal decodable mono 16 bit PCM WAV of exactly one second with the
/// given RIFF INFO subchunks, one `(id, value)` pair per tag.
///
/// Shared by the metadata writer and the app integration tests so every
/// fixture builder stays in one place; the metadata reader tests keep their
/// own private builder for historical reasons.
pub fn wav_bytes(tags: &[(&str, &str)]) -> Vec<u8> {
    const SAMPLE_RATE: u32 = 44100;
    const SAMPLES: usize = SAMPLE_RATE as usize;

    let mut fmt_payload = Vec::new();
    fmt_payload.extend_from_slice(&1_u16.to_le_bytes()); // PCM
    fmt_payload.extend_from_slice(&1_u16.to_le_bytes()); // mono
    fmt_payload.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    fmt_payload.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // byte rate
    fmt_payload.extend_from_slice(&2_u16.to_le_bytes()); // block align
    fmt_payload.extend_from_slice(&16_u16.to_le_bytes()); // bits

    let data_payload = vec![0_u8; SAMPLES * 2];

    let mut info = b"INFO".to_vec();
    for (id, value) in tags {
        let mut payload = value.as_bytes().to_vec();
        payload.push(0);
        let mut chunk = Vec::with_capacity(8 + payload.len());
        chunk.extend_from_slice(id.as_bytes());
        chunk.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        chunk.extend_from_slice(&payload);
        if payload.len() % 2 == 1 {
            chunk.push(0);
        }
        info.extend(chunk);
    }

    let mut body = b"WAVE".to_vec();
    let mut fmt_chunk = Vec::with_capacity(8 + fmt_payload.len());
    fmt_chunk.extend_from_slice(b"fmt ");
    fmt_chunk.extend_from_slice(&(fmt_payload.len() as u32).to_le_bytes());
    fmt_chunk.extend_from_slice(&fmt_payload);
    body.extend(fmt_chunk);
    if info.len() > 4 {
        let mut list_chunk = Vec::with_capacity(8 + info.len());
        list_chunk.extend_from_slice(b"LIST");
        list_chunk.extend_from_slice(&(info.len() as u32).to_le_bytes());
        list_chunk.extend_from_slice(&info);
        body.extend(list_chunk);
    }
    let mut data_chunk = Vec::with_capacity(8 + data_payload.len());
    data_chunk.extend_from_slice(b"data");
    data_chunk.extend_from_slice(&(data_payload.len() as u32).to_le_bytes());
    data_chunk.extend_from_slice(&data_payload);
    body.extend(data_chunk);

    let mut file = b"RIFF".to_vec();
    file.extend_from_slice(&(body.len() as u32).to_le_bytes());
    file.extend_from_slice(&body);
    file
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> reqwest::blocking::Client {
        reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .expect("test HTTP client")
    }

    #[test]
    fn temp_dir_owns_cleanup_and_preserves_path_convenience() {
        let fixture = unique_temp_dir("lifecycle");
        let path = fixture.path().to_path_buf();

        assert!(fixture.as_ref().is_dir());
        assert_eq!(fixture.as_ref(), &*fixture);
        assert_eq!(fixture.join("nested"), path.join("nested"));

        drop(fixture);
        assert!(!path.exists());
    }

    #[test]
    fn temp_dir_cleanup_reports_a_blocking_path() {
        let fixture = unique_temp_dir("cleanup-failure");
        let path = fixture.path().to_path_buf();

        fs::remove_dir_all(&path).expect("remove fixture directory");
        fs::write(&path, b"blocking path").expect("create blocking path");

        let error = fixture
            .cleanup()
            .expect_err("a file must block directory cleanup");
        assert_eq!(error.kind(), io::ErrorKind::NotADirectory);

        fs::remove_file(path).expect("remove blocking path");
    }

    #[test]
    fn scripted_server_preserves_status_headers_and_fixed_body() {
        let server =
            ScriptedHttpServer::new([ScriptedHttpResponse::fixed(201, b"created")
                .with_header("Content-Type", "text/plain")]);
        let response = client().get(server.url()).send().expect("response");

        assert_eq!(response.status(), reqwest::StatusCode::CREATED);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/plain"
        );
        assert_eq!(response.bytes().expect("body"), b"created".as_slice());
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn scripted_server_writes_delayed_chunked_bodies() {
        let server = ScriptedHttpServer::new([ScriptedHttpResponse::chunked(
            200,
            [b"first".as_slice(), b"second".as_slice()],
            Duration::from_millis(1),
        )]);
        let response = client().get(server.url()).send().expect("response");
        assert_eq!(response.bytes().expect("body"), b"firstsecond".as_slice());
    }

    #[test]
    fn scripted_server_surfaces_partial_and_malformed_responses() {
        let server = ScriptedHttpServer::new([
            ScriptedHttpResponse::close_during_body(200, b"partial"),
            ScriptedHttpResponse::malformed(b"not an HTTP response\r\n"),
        ]);
        let first = client().get(server.url()).send().expect("partial response");
        assert!(first.bytes().is_err(), "truncated body must fail the read");
        assert!(client().get(server.url()).send().is_err());
    }

    #[test]
    fn scripted_server_drop_has_bounded_shutdown() {
        let started = std::time::Instant::now();
        drop(ScriptedHttpServer::new(std::iter::empty()));
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "idle fixture shutdown must stay bounded: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn scripted_server_drop_interrupts_an_arbitrarily_long_chunk_delay() {
        let server = ScriptedHttpServer::new([ScriptedHttpResponse::chunked(
            200,
            [b"first".as_slice(), b"second".as_slice()],
            Duration::from_secs(86_400),
        )]);
        let mut client = TcpStream::connect(server.address).expect("connect scripted server");
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .expect("send scripted request");

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while server.requests().is_empty() && std::time::Instant::now() < deadline {
            thread::sleep(SCRIPTED_HTTP_POLL_INTERVAL);
        }
        assert_eq!(server.requests().len(), 1, "server must start the response");

        let started = std::time::Instant::now();
        drop(server);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "long scripted delays must not block fixture teardown: {:?}",
            started.elapsed()
        );
    }
}
