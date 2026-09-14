//! Public YouTube provider backed by `yt-dlp`.
//!
//! `yt-dlp` is the de-facto resolver for public YouTube URLs: it negotiates
//! the format table, picks an audio-only stream and hands back a direct
//! media URL. We use it in two modes:
//!
//! - **Metadata**: `yt-dlp --dump-json --skip-download <url>` returns a JSON
//!   blob with the title, duration, channel, thumbnail URL and the
//!   available formats. The provider extracts the fields the spec cares
//!   about and discards the rest.
//! - **Playback**: `yt-dlp -f bestaudio -g <url>` prints the resolved media
//!   URL on stdout, which the provider then hands to the generic HTTP
//!   reader so the audio engine only ever sees a homogeneous byte source.
//!
//! The YouTube URL stored in the playlist is the **original watch URL** (the
//! one the user typed or that was in the `.m3u8`). The direct media URL is
//! never persisted: it can expire between sessions, so the player resolves
//! it lazily on playback.
//!
//! `yt-dlp` is treated as an optional dependency. When the binary is
//! missing, the provider returns [`StreamError::MissingTool`] and the
//! resolver surfaces a clean notification to the user instead of panicking.

use std::io::Read;
use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::fd::AsRawFd;

use url::Url;

use crate::stream::provider::{
    ResolvedStream, StreamCancellation, StreamError, StreamProvider, StreamReader,
};
use crate::stream::source::StreamKind;
use crate::stream::url_detect::is_youtube_host;

const YTDLP_TIMEOUT: Duration = Duration::from_secs(20);
const YTDLP_STDOUT_LIMIT: usize = 2 * 1024 * 1024;
const YTDLP_STDERR_LIMIT: usize = 16 * 1024;
const YTDLP_DIAGNOSTIC_LIMIT: usize = 512;
const YTDLP_PIPE_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);

/// Resolves public YouTube URLs through `yt-dlp`.
#[derive(Debug, Default, Clone)]
pub struct YouTubeProvider {
    _private: (),
}

impl YouTubeProvider {
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl StreamProvider for YouTubeProvider {
    fn can_handle(&self, url: &Url) -> bool {
        url.host_str()
            .map(|host| is_youtube_host(&host.to_ascii_lowercase()))
            .unwrap_or(false)
    }

    fn kind(&self) -> StreamKind {
        StreamKind::YouTube
    }

    fn resolve(
        &self,
        url: &Url,
        cancellation: &StreamCancellation,
    ) -> Result<ResolvedStream, StreamError> {
        let json = run_ytdlp_capture_cancellable(
            Path::new("yt-dlp"),
            &["--dump-json", "--skip-download"],
            url,
            cancellation,
        )?;
        if cancellation.is_cancelled() {
            return Err(StreamError::Cancelled);
        }
        parse_metadata_json(&json, url)
    }

    fn open_reader(
        &self,
        url: &Url,
        cancellation: &StreamCancellation,
    ) -> Result<StreamReader, StreamError> {
        // We do not return a Reader from yt-dlp directly; we resolve the
        // direct media URL and let the generic HTTP reader open the body.
        // That keeps the playback path homogeneous and means the audio
        // worker is the only place that owns a long-lived network socket.
        let direct = resolve_direct_url(url, Some(cancellation))?;
        crate::stream::http::HttpProvider::new().open_reader(&direct, cancellation)
    }
}

fn parse_metadata_json(json: &str, url: &Url) -> Result<ResolvedStream, StreamError> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|error| StreamError::External {
            url: url.clone(),
            message: format!("yt-dlp returned invalid JSON: {error}"),
        })?;

    let title = value
        .get("title")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let duration = value
        .get("duration")
        .and_then(|v| v.as_f64())
        .and_then(duration_from_secs);
    let channel = value
        .get("channel")
        .or_else(|| value.get("uploader"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let thumbnail = value
        .get("thumbnail")
        .and_then(|v| v.as_str())
        .and_then(|s| Url::parse(s).ok());
    let webpage_url = value
        .get("webpage_url")
        .or_else(|| value.get("original_url"))
        .and_then(|v| v.as_str())
        .and_then(|s| Url::parse(s).ok());

    let mut resolved = ResolvedStream {
        title,
        duration,
        station: channel,
        logo_url: thumbnail,
        ..ResolvedStream::default()
    };
    if resolved.homepage.is_none() {
        resolved.homepage = webpage_url;
    }
    Ok(resolved)
}

fn duration_from_secs(secs: f64) -> Option<Duration> {
    if !secs.is_finite() || secs < 0.0 {
        return None;
    }
    Duration::try_from_secs_f64(secs).ok()
}

/// Run `yt-dlp` with the given extra args and capture stdout.
///
/// Centralised so every call uses the same timeouts and the same error
/// reporting. The `--no-warnings`, `--no-playlist` and `--no-progress`
/// flags keep stderr noise out of the notification path and avoid pulling
/// every entry of a playlist when the user pasted a watch URL.
fn run_ytdlp_capture(extra: &[&str], url: &Url) -> Result<String, StreamError> {
    run_ytdlp_capture_with_limits_and_control(
        Path::new("yt-dlp"),
        extra,
        url,
        YTDLP_TIMEOUT,
        YTDLP_STDOUT_LIMIT,
        YTDLP_STDERR_LIMIT,
        None,
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    )
}

fn run_ytdlp_capture_cancellable(
    program: &Path,
    extra: &[&str],
    url: &Url,
    cancellation: &StreamCancellation,
) -> Result<String, StreamError> {
    run_ytdlp_capture_with_limits_and_control(
        program,
        extra,
        url,
        YTDLP_TIMEOUT,
        YTDLP_STDOUT_LIMIT,
        YTDLP_STDERR_LIMIT,
        Some(cancellation),
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    )
}

#[cfg(test)]
fn run_ytdlp_capture_with_limits(
    program: &Path,
    extra: &[&str],
    url: &Url,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> Result<String, StreamError> {
    run_ytdlp_capture_with_limits_and_control(
        program,
        extra,
        url,
        timeout,
        stdout_limit,
        stderr_limit,
        None,
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    )
}

fn run_ytdlp_capture_with_limits_and_control(
    program: &Path,
    extra: &[&str],
    url: &Url,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
    cancellation: Option<&StreamCancellation>,
    active_capture_threads: Arc<std::sync::atomic::AtomicUsize>,
) -> Result<String, StreamError> {
    if cancellation.is_some_and(StreamCancellation::is_cancelled) {
        return Err(StreamError::Cancelled);
    }

    let mut command = Command::new(program);
    command
        .arg("--no-warnings")
        .arg("--no-playlist")
        .arg("--no-progress");
    for arg in extra {
        command.arg(arg);
    }
    command.arg(url.as_str());

    configure_process_group(&mut command);

    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => StreamError::MissingTool("yt-dlp".to_string()),
            _ => StreamError::External {
                url: url.clone(),
                message: crate::net::normalize_diagnostic(
                    &error.to_string(),
                    YTDLP_DIAGNOSTIC_LIMIT,
                ),
            },
        })?;
    let mut child = ChildGuard::new(child);

    let stdout = match child.take_stdout() {
        Some(stdout) => stdout,
        None => {
            return Err(StreamError::External {
                url: url.clone(),
                message: "yt-dlp stdout pipe was not available".into(),
            });
        }
    };
    let stderr = match child.take_stderr() {
        Some(stderr) => stderr,
        None => {
            drop(stdout);
            return Err(StreamError::External {
                url: url.clone(),
                message: "yt-dlp stderr pipe was not available".into(),
            });
        }
    };

    if let Err(error) =
        configure_capture_pipe(&stdout).and_then(|()| configure_capture_pipe(&stderr))
    {
        return Err(StreamError::External {
            url: url.clone(),
            message: crate::net::normalize_diagnostic(
                &format!("could not configure yt-dlp capture pipe: {error}"),
                YTDLP_DIAGNOSTIC_LIMIT,
            ),
        });
    }

    let stdout_overflow = Arc::new(AtomicBool::new(false));
    let stdout_task = CaptureTask::spawn(
        stdout,
        stdout_limit,
        true,
        Some(stdout_overflow.clone()),
        active_capture_threads.clone(),
    );
    let stderr_task = CaptureTask::spawn(stderr, stderr_limit, false, None, active_capture_threads);

    let started = Instant::now();
    let mut status = None;
    let mut timed_out = false;
    let mut cancelled = false;
    let mut wait_error = None;
    let mut pipes_lingering = false;
    loop {
        if stdout_overflow.load(Ordering::Acquire) {
            child.terminate();
            break;
        }
        if cancellation.is_some_and(StreamCancellation::is_cancelled) {
            cancelled = true;
            child.terminate();
            break;
        }
        match child.try_wait() {
            Ok(Some(exit)) => {
                status = Some(exit);
                break;
            }
            Ok(None) if started.elapsed() >= timeout => {
                timed_out = true;
                child.terminate();
                break;
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                child.terminate();
                wait_error = Some(error);
                break;
            }
        }
    }

    // A direct child can exit while a descendant still owns an inherited
    // pipe. Never join the capture threads before a bounded drain wait: such
    // a descendant would otherwise hang the resolver indefinitely.
    let mut stdout_result = None;
    let mut stderr_result = None;
    wait_for_captures(
        &stdout_task.receiver,
        &stderr_task.receiver,
        &mut stdout_result,
        &mut stderr_result,
        YTDLP_PIPE_DRAIN_TIMEOUT,
    );
    if stdout_result.is_none() || stderr_result.is_none() {
        pipes_lingering = true;
        child.terminate();
        wait_for_captures(
            &stdout_task.receiver,
            &stderr_task.receiver,
            &mut stdout_result,
            &mut stderr_result,
            YTDLP_PIPE_DRAIN_TIMEOUT,
        );
    }

    if stdout_result.is_none() || stderr_result.is_none() {
        stdout_task.cancel();
        stderr_task.cancel();
        wait_for_captures(
            &stdout_task.receiver,
            &stderr_task.receiver,
            &mut stdout_result,
            &mut stderr_result,
            YTDLP_PIPE_DRAIN_TIMEOUT,
        );
    }

    if status.is_some() {
        child.disarm();
    }

    let join_result = join_capture_threads(stdout_task, stderr_task, url);
    if let Err(error) = join_result {
        return Err(error);
    }
    if cancellation.is_some_and(StreamCancellation::is_cancelled) {
        return Err(StreamError::Cancelled);
    }
    if let Some(error) = wait_error {
        return Err(StreamError::External {
            url: url.clone(),
            message: crate::net::normalize_diagnostic(
                &format!("could not wait for yt-dlp: {error}"),
                YTDLP_DIAGNOSTIC_LIMIT,
            ),
        });
    }
    if cancelled {
        return Err(StreamError::Cancelled);
    }
    if timed_out || pipes_lingering {
        return Err(capture_timeout(url, timeout));
    }
    if stdout_overflow.load(Ordering::Acquire) {
        return Err(StreamError::ExternalOutputLimit {
            url: url.clone(),
            stream: "stdout",
            limit: stdout_limit,
        });
    }

    let stdout = match stdout_result {
        Some(Ok(capture)) => capture,
        Some(Err(error)) => return Err(capture_error(url, "stdout", error)),
        None => return Err(capture_timeout(url, timeout)),
    };
    let stderr = match stderr_result {
        Some(Ok(capture)) => capture,
        Some(Err(error)) => return Err(capture_error(url, "stderr", error)),
        None => return Err(capture_timeout(url, timeout)),
    };
    if stdout.exceeded {
        return Err(StreamError::ExternalOutputLimit {
            url: url.clone(),
            stream: "stdout",
            limit: stdout_limit,
        });
    }

    let status = status.ok_or_else(|| StreamError::External {
        url: url.clone(),
        message: "yt-dlp exited without a status".into(),
    })?;
    if !status.success() {
        let stderr = crate::net::normalize_diagnostic(
            &String::from_utf8_lossy(&stderr.bytes),
            YTDLP_DIAGNOSTIC_LIMIT,
        );
        return Err(StreamError::External {
            url: url.clone(),
            message: if stderr.is_empty() {
                format!("yt-dlp exited with status {status}")
            } else {
                stderr
            },
        });
    }
    Ok(String::from_utf8_lossy(&stdout.bytes).into_owned())
}

struct ProcessCapture {
    bytes: Vec<u8>,
    exceeded: bool,
}

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.as_mut()?.stdout.take()
    }

    fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.as_mut()?.stderr.take()
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child.as_mut().map_or(Ok(None), Child::try_wait)
    }

    fn terminate(&mut self) {
        if let Some(mut child) = self.child.take() {
            terminate_and_reap(&mut child);
        }
    }

    fn disarm(&mut self) {
        let _ = self.child.take();
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

struct CaptureTask {
    cancellation: Arc<AtomicBool>,
    receiver: Receiver<std::io::Result<ProcessCapture>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl CaptureTask {
    fn spawn(
        reader: impl Read + Send + 'static,
        limit: usize,
        stop_on_overflow: bool,
        overflow: Option<Arc<AtomicBool>>,
        active_threads: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        let cancellation = Arc::new(AtomicBool::new(false));
        let cancellation_for_thread = Arc::clone(&cancellation);
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            active_threads.fetch_add(1, Ordering::AcqRel);
            let _active = ActiveCaptureThread(active_threads);
            let result =
                read_process_output(reader, limit, stop_on_overflow, cancellation_for_thread);
            if let (Some(overflow), Ok(capture)) = (&overflow, &result) {
                if capture.exceeded {
                    overflow.store(true, Ordering::Release);
                }
            }
            let _ = sender.send(result);
        });
        Self {
            cancellation,
            receiver,
            handle: Some(handle),
        }
    }

    fn cancel(&self) {
        self.cancellation.store(true, Ordering::Release);
    }

    fn join(&mut self, url: &Url, stream: &str) -> Result<(), StreamError> {
        if let Some(handle) = self.handle.take() {
            handle.join().map_err(|_| StreamError::External {
                url: url.clone(),
                message: format!("yt-dlp {stream} capture thread failed"),
            })
        } else {
            Err(StreamError::External {
                url: url.clone(),
                message: format!("yt-dlp {stream} capture thread was already joined"),
            })
        }
    }
}

struct ActiveCaptureThread(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for ActiveCaptureThread {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn wait_for_captures(
    stdout_receiver: &Receiver<std::io::Result<ProcessCapture>>,
    stderr_receiver: &Receiver<std::io::Result<ProcessCapture>>,
    stdout_result: &mut Option<std::io::Result<ProcessCapture>>,
    stderr_result: &mut Option<std::io::Result<ProcessCapture>>,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    while stdout_result.is_none() || stderr_result.is_none() {
        if stdout_result.is_none() {
            match stdout_receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(result) => *stdout_result = Some(result),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    *stdout_result = Some(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "stdout capture thread disconnected",
                    )))
                }
            }
        }
        if stderr_result.is_none() {
            match stderr_receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(result) => *stderr_result = Some(result),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    *stderr_result = Some(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "stderr capture thread disconnected",
                    )))
                }
            }
        }
    }
}

fn join_capture_threads(
    mut stdout: CaptureTask,
    mut stderr: CaptureTask,
    url: &Url,
) -> Result<(), StreamError> {
    let stdout_result = stdout.join(url, "stdout");
    let stderr_result = stderr.join(url, "stderr");
    stdout_result.and(stderr_result)
}

fn read_process_output(
    mut reader: impl Read,
    limit: usize,
    stop_on_overflow: bool,
    cancellation: Arc<AtomicBool>,
) -> std::io::Result<ProcessCapture> {
    let mut bytes = Vec::with_capacity(limit.min(16 * 1024));
    let mut chunk = [0u8; 8 * 1024];
    let mut exceeded = false;
    loop {
        if cancellation.load(Ordering::Acquire) {
            return Ok(ProcessCapture { bytes, exceeded });
        }
        let read = match reader.read(&mut chunk) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(error) => return Err(error),
        };
        if read == 0 {
            return Ok(ProcessCapture { bytes, exceeded });
        }
        let remaining = limit.saturating_sub(bytes.len());
        bytes.extend_from_slice(&chunk[..read.min(remaining)]);
        if read > remaining {
            exceeded = true;
            if stop_on_overflow {
                return Ok(ProcessCapture { bytes, exceeded });
            }
        }
    }
}

fn capture_error(url: &Url, stream: &str, error: std::io::Error) -> StreamError {
    StreamError::External {
        url: url.clone(),
        message: crate::net::normalize_diagnostic(
            &format!("could not read yt-dlp {stream}: {error}"),
            YTDLP_DIAGNOSTIC_LIMIT,
        ),
    }
}

fn capture_timeout(url: &Url, timeout: Duration) -> StreamError {
    StreamError::ExternalTimeout {
        url: url.clone(),
        timeout_ms: timeout.as_millis().min(u128::from(u64::MAX)) as u64,
    }
}

#[cfg(unix)]
fn configure_capture_pipe(reader: &impl AsRawFd) -> std::io::Result<()> {
    let fd = reader.as_raw_fd();
    // Non-blocking reads give cancellation a bounded join point. The worker
    // owns the read handle and drops it when it observes cancellation.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn configure_capture_pipe<T>(_reader: &T) -> std::io::Result<()> {
    Ok(())
}

fn terminate_and_reap(child: &mut Child) {
    #[cfg(unix)]
    {
        // The resolver is spawned as a session leader, so a negative PID
        // targets the whole provider process group, including shell helpers
        // that may still own stdout or stderr.
        if let Some(process_group) = checked_process_group_id(child.id()) {
            unsafe {
                let _ = libc::kill(-process_group, libc::SIGKILL);
            }
        }
    }
    let _ = child.kill();
    // Killing is immediately followed by a wait, making this the child reap
    // boundary. Capture workers are joined after their pipes drain or cancel.
    let _ = child.wait();
}

#[cfg(unix)]
fn checked_process_group_id(raw_pid: u32) -> Option<libc::pid_t> {
    let pid = libc::pid_t::try_from(raw_pid).ok()?;
    (pid > 0).then_some(pid)
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

/// Resolve the direct media URL behind a YouTube watch URL.
fn resolve_direct_url(
    url: &Url,
    cancellation: Option<&StreamCancellation>,
) -> Result<Url, StreamError> {
    let raw = match cancellation {
        Some(cancellation) => run_ytdlp_capture_cancellable(
            Path::new("yt-dlp"),
            &["-f", "bestaudio", "-g"],
            url,
            cancellation,
        )?,
        None => run_ytdlp_capture(&["-f", "bestaudio", "-g"], url)?,
    };
    let first = raw.lines().next().unwrap_or("").trim();
    parse_direct_url(first, url)
}

fn parse_direct_url(raw: &str, source_url: &Url) -> Result<Url, StreamError> {
    let direct = Url::parse(raw).map_err(|error| StreamError::Unsupported {
        url: source_url.clone(),
        message: format!("yt-dlp returned a malformed direct URL: {error}"),
    })?;
    if !crate::stream::http::is_valid_http_url(&direct) {
        return Err(StreamError::Unsupported {
            url: source_url.clone(),
            message: "yt-dlp returned a direct URL that is not http/https with a host".into(),
        });
    }
    Ok(direct)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn youtube_provider_recognises_youtube_hosts() {
        let provider = YouTubeProvider::new();
        assert!(provider.can_handle(&Url::parse("https://youtu.be/abc").unwrap()));
        assert!(provider.can_handle(&Url::parse("https://www.youtube.com/watch?v=abc").unwrap()));
        assert!(!provider.can_handle(&Url::parse("https://example.com/").unwrap()));
        assert_eq!(provider.kind(), StreamKind::YouTube);
    }

    /// The `resolve` and `open_reader` paths both shell out to `yt-dlp`, so
    /// we do not exercise them here; instead the integration test in
    /// `resolver.rs` covers the orchestration through a fake provider.
    #[test]
    fn resolution_is_skipped_when_yt_dlp_is_missing() {
        let url = Url::parse("https://www.youtube.com/watch?v=abc").expect("valid URL");
        let error = run_ytdlp_capture_with_limits(
            Path::new("this-binary-does-not-exist"),
            &["--dump-json"],
            &url,
            Duration::from_millis(100),
            1024,
            1024,
        )
        .expect_err("missing executable");
        assert!(matches!(error, StreamError::MissingTool(_)));
    }

    #[test]
    fn fake_ytdlp_missing_metadata_degrades_to_an_empty_resolution() {
        let (directory, program) = fake_executable("printf '%s' '{}'");
        let url = Url::parse("https://www.youtube.com/watch?v=abc").expect("valid URL");
        let json = run_ytdlp_capture_with_limits(
            &program,
            &["--dump-json"],
            &url,
            Duration::from_secs(1),
            1024,
            1024,
        )
        .expect("fake metadata");

        let resolved = parse_metadata_json(&json, &url).expect("empty metadata is valid JSON");
        assert_eq!(resolved.title, None);
        assert_eq!(resolved.station, None);
        drop(directory);
    }

    #[test]
    fn duration_parsing_preserves_valid_and_unknown_semantics() {
        let url = Url::parse("https://www.youtube.com/watch?v=abc").expect("valid URL");
        let normal = parse_metadata_json(r#"{"duration": 123.5}"#, &url).expect("metadata");
        assert_eq!(normal.duration, Some(Duration::from_millis(123_500)));

        let live = parse_metadata_json(r#"{"duration": null}"#, &url).expect("metadata");
        assert_eq!(live.duration, None);
    }

    #[test]
    fn duration_parsing_rejects_negative_and_huge_finite_values() {
        assert_eq!(duration_from_secs(-1.0), None);
        assert_eq!(duration_from_secs(1e300), None);
        assert_eq!(duration_from_secs(f64::NAN), None);
        assert_eq!(duration_from_secs(f64::INFINITY), None);
    }

    #[test]
    fn direct_url_parser_allows_http_and_https_cdn_urls() {
        let source = Url::parse("https://www.youtube.com/watch?v=abc").expect("source URL");
        for raw in [
            "http://cdn.example.test/audio.mp4?signature=secret",
            "https://r3---sn.example.test/audio.webm?token=secret",
        ] {
            let direct = parse_direct_url(raw, &source).expect("valid CDN URL");
            assert_eq!(direct.scheme(), Url::parse(raw).unwrap().scheme());
            assert!(direct.host_str().is_some());
        }
    }

    #[test]
    fn direct_url_parser_rejects_malformed_unsupported_and_hostless_urls() {
        let source = Url::parse("https://www.youtube.com/watch?v=secret").expect("source URL");
        for raw in ["not a URL", "ftp://cdn.example.test/audio.mp3", "http://"] {
            let error = parse_direct_url(raw, &source).expect_err("invalid direct URL");
            assert!(matches!(error, StreamError::Unsupported { .. }));
            assert!(!error.to_string().contains("secret"));
        }
    }

    #[test]
    fn fake_ytdlp_malformed_metadata_is_rejected() {
        let (directory, program) = fake_executable("printf '%s' 'not-json'");
        let url = Url::parse("https://www.youtube.com/watch?v=abc").expect("valid URL");
        let json = run_ytdlp_capture_with_limits(
            &program,
            &["--dump-json"],
            &url,
            Duration::from_secs(1),
            1024,
            1024,
        )
        .expect("fake metadata process");

        let error = parse_metadata_json(&json, &url).expect_err("malformed JSON");
        assert!(matches!(error, StreamError::External { .. }));
        drop(directory);
    }

    #[test]
    fn fake_ytdlp_non_zero_exit_is_reported_without_parsing_stdout() {
        let (directory, program) = fake_executable("printf '%s' 'failed' >&2; exit 7");
        let url = Url::parse("https://www.youtube.com/watch?v=abc").expect("valid URL");
        let error = run_ytdlp_capture_with_limits(
            &program,
            &["--dump-json"],
            &url,
            Duration::from_secs(1),
            1024,
            1024,
        )
        .expect_err("non-zero fake process");

        assert!(matches!(error, StreamError::External { .. }));
        assert!(error.to_string().contains("failed"));
        drop(directory);
    }

    #[test]
    fn fake_ytdlp_output_is_preserved_without_real_installation() {
        let (directory, program) = fake_executable("printf '%s' 'https://media.example/live\\n'");
        let url = Url::parse("https://www.youtube.com/watch?v=abc").expect("valid URL");
        let output =
            run_ytdlp_capture_with_limits(&program, &[], &url, Duration::from_secs(1), 1024, 1024)
                .expect("fake output");
        assert_eq!(output, "https://media.example/live\\n");
        drop(directory);
    }

    #[test]
    fn fake_ytdlp_descendant_holding_pipes_is_killed_after_timeout() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let marker = directory.path().join("descendant-survived");
        let script = format!(
            "(sleep 0.2; printf survived > '{}') & wait",
            marker.display()
        );
        let program = fake_executable_in(&directory, &script);
        let url = Url::parse("https://www.youtube.com/watch?v=abc").expect("valid URL");
        let started = Instant::now();
        let error = run_ytdlp_capture_with_limits(
            &program,
            &[],
            &url,
            Duration::from_millis(50),
            1024,
            1024,
        )
        .expect_err("timeout");
        assert!(
            matches!(error, StreamError::ExternalTimeout { .. }),
            "got {error:?}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "terminating the provider tree must release pipe readers promptly: {:?}",
            started.elapsed()
        );
        thread::sleep(Duration::from_millis(250));
        assert!(
            !marker.exists(),
            "a descendant that outlives the resolver must not survive process-group cleanup"
        );
        drop(directory);
    }

    #[test]
    fn fake_ytdlp_timeout_joins_capture_threads_before_return() {
        let (directory, program) = fake_executable("sleep 2 & wait");
        let url = Url::parse("https://www.youtube.com/watch?v=abc").expect("valid URL");
        let active_threads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let error = run_ytdlp_capture_with_limits_and_control(
            &program,
            &[],
            &url,
            Duration::from_millis(50),
            1024,
            1024,
            None,
            active_threads.clone(),
        )
        .expect_err("timeout");

        assert!(matches!(error, StreamError::ExternalTimeout { .. }));
        assert_eq!(active_threads.load(Ordering::Acquire), 0);
        drop(directory);
    }

    #[test]
    fn fake_ytdlp_cancellation_joins_capture_threads_and_kills_descendants() {
        let (directory, program) = fake_executable("sleep 2 & wait");
        let url = Url::parse("https://www.youtube.com/watch?v=abc").expect("valid URL");
        let cancellation = StreamCancellation::new();
        let cancellation_for_worker = cancellation.clone();
        let active_threads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let active_threads_for_worker = active_threads.clone();
        let started = Instant::now();
        let worker = thread::spawn(move || {
            run_ytdlp_capture_with_limits_and_control(
                &program,
                &[],
                &url,
                Duration::from_secs(2),
                1024,
                1024,
                Some(&cancellation_for_worker),
                active_threads_for_worker,
            )
        });

        thread::sleep(Duration::from_millis(50));
        cancellation.cancel();
        let error = worker
            .join()
            .expect("capture worker must join")
            .expect_err("cancelled");

        assert!(matches!(error, StreamError::Cancelled));
        assert_eq!(active_threads.load(Ordering::Acquire), 0);
        assert!(started.elapsed() < Duration::from_millis(500));
        drop(directory);
    }

    #[test]
    fn cancellable_ytdlp_capture_wrapper_reaps_the_fake_process_group() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let marker = directory.path().join("wrapper-descendant-survived");
        let script = format!(
            "(sleep 0.2; printf survived > '{}') & wait",
            marker.display()
        );
        let program = fake_executable_in(&directory, &script);
        let url = Url::parse("https://www.youtube.com/watch?v=abc").expect("valid URL");
        let cancellation = StreamCancellation::new();
        let cancellation_for_worker = cancellation.clone();
        let worker = thread::spawn(move || {
            run_ytdlp_capture_cancellable(&program, &[], &url, &cancellation_for_worker)
        });

        thread::sleep(Duration::from_millis(50));
        cancellation.cancel();
        let error = worker
            .join()
            .expect("capture worker must join")
            .expect_err("cancelled");

        assert!(matches!(error, StreamError::Cancelled));
        thread::sleep(Duration::from_millis(250));
        assert!(!marker.exists(), "cancelled capture must reap descendants");
        drop(directory);
    }

    #[cfg(unix)]
    #[test]
    fn child_guard_kills_process_group_when_dropped_early() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let marker = directory.path().join("descendant-survived");
        let script = format!(
            "(sleep 0.2; printf survived > '{}') & wait",
            marker.display()
        );
        let program = fake_executable_in(&directory, &script);
        let mut command = Command::new(&program);
        configure_process_group(&mut command);
        let child = command
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("fake process");

        {
            let _guard = ChildGuard::new(child);
            thread::sleep(Duration::from_millis(50));
        }

        thread::sleep(Duration::from_millis(250));
        assert!(!marker.exists(), "dropping the guard must kill descendants");
    }

    #[test]
    fn fake_ytdlp_exited_child_with_lingering_pipe_is_bounded() {
        let (directory, program) = fake_executable("sleep 2 & kill -TERM $$");
        let url = Url::parse("https://www.youtube.com/watch?v=abc").expect("valid URL");
        let started = Instant::now();
        let error =
            run_ytdlp_capture_with_limits(&program, &[], &url, Duration::from_secs(1), 1024, 1024)
                .expect_err("lingering descendant pipe");
        assert!(
            matches!(error, StreamError::ExternalTimeout { .. }),
            "got {error:?}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(800),
            "an inherited pipe must not make a completed child hang: {:?}",
            started.elapsed()
        );
        drop(directory);
    }

    #[test]
    fn capture_task_double_join_returns_typed_error() {
        let active_threads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut task = CaptureTask::spawn(
            std::io::Cursor::new(b"output".to_vec()),
            1024,
            false,
            None,
            active_threads.clone(),
        );
        let url = Url::parse("https://www.youtube.com/watch?v=abc").expect("valid URL");

        task.join(&url, "stdout").expect("first join");
        let error = task.join(&url, "stdout").expect_err("second join");

        assert!(matches!(error, StreamError::External { .. }));
        assert!(error.to_string().contains("already joined"));
        assert_eq!(active_threads.load(Ordering::Acquire), 0);
    }

    #[test]
    fn fake_ytdlp_stdout_overflow_is_typed_and_bounded() {
        let (directory, program) = fake_executable("head -c 4096 /dev/zero");
        let url = Url::parse("https://www.youtube.com/watch?v=abc").expect("valid URL");
        let error =
            run_ytdlp_capture_with_limits(&program, &[], &url, Duration::from_secs(1), 128, 1024)
                .expect_err("stdout overflow");
        assert!(matches!(error, StreamError::ExternalOutputLimit { .. }));
        drop(directory);
    }

    #[test]
    fn fake_ytdlp_stderr_is_redacted_and_truncated_before_surface() {
        let (directory, program) = fake_executable(
            "printf '%s\\n' 'https://cdn.example/live?token=secret' >&2; head -c 2048 /dev/zero >&2; exit 1",
        );
        let url = Url::parse("https://www.youtube.com/watch?v=secret").expect("valid URL");
        let error =
            run_ytdlp_capture_with_limits(&program, &[], &url, Duration::from_secs(1), 1024, 256)
                .expect_err("fake stderr failure");
        let rendered = error.to_string();
        assert!(!rendered.contains("token=secret"));
        assert!(!rendered.contains("watch?v=secret"));
        assert!(rendered.len() < 800, "stderr must be bounded: {rendered}");
        drop(directory);
    }

    #[cfg(unix)]
    #[test]
    fn process_group_pid_conversion_is_checked() {
        assert_eq!(checked_process_group_id(0), None);
        assert_eq!(checked_process_group_id(1), Some(1));
        if libc::pid_t::try_from(u32::MAX).is_err() {
            assert_eq!(checked_process_group_id(u32::MAX), None);
        }
    }

    fn fake_executable(script: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let program = fake_executable_in(&directory, script);
        (directory, program)
    }

    fn fake_executable_in(directory: &tempfile::TempDir, script: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let program = directory.path().join("yt-dlp-fake");
        std::fs::write(&program, format!("#!/bin/sh\n{script}\n")).expect("write fake");
        let mut permissions = std::fs::metadata(&program)
            .expect("fake metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&program, permissions).expect("make fake executable");
        program
    }
}
