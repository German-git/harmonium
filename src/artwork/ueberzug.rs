//! Optional original ueberzug image-layer integration.
//!
//! This adapter is intentionally separate from [`super::ueberzugpp`]. The
//! original project is an X11-oriented process with its own parser option and
//! legacy scaler vocabulary, so its command line and payload contract must not
//! be inferred from ueberzugpp's implementation.

use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::backend::{ArtworkOverlay, ExternalArtworkLayer, ExternalProcess};

/// Build the original ueberzug invocation with its explicit JSON parser mode.
pub(crate) fn command() -> Command {
    let mut command = Command::new("ueberzug");
    command
        .args(["layer", "--parser", "json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// Build an original ueberzug `add` JSON object.
///
/// `contain` is the legacy scaler name. It preserves the aspect ratio inside
/// the requested placement without relying on ueberzugpp-only semantics.
pub fn add_command(overlay: &ArtworkOverlay) -> Value {
    add_command_with_calibration(overlay, None)
}

fn add_command_with_calibration(
    overlay: &ArtworkOverlay,
    calibration: Option<CoordinateCalibration>,
) -> Value {
    let overlay = calibration
        .map(|calibration| calibration.transform(overlay))
        .unwrap_or_else(|| overlay.clone());
    json!({
        "action": "add",
        "identifier": overlay.identifier,
        "path": overlay.path,
        "x": overlay.rect.x,
        "y": overlay.rect.y,
        "width": overlay.rect.width,
        "height": overlay.rect.height,
        "scaler": "contain",
    })
}

/// Build an original ueberzug `remove` JSON object.
pub fn remove_command(identifier: &str) -> Value {
    json!({
        "action": "remove",
        "identifier": identifier,
    })
}

const XWININFO_OUTPUT_LIMIT: usize = 64 * 1024;
const XWININFO_TIMEOUT: Duration = Duration::from_millis(100);
const XWININFO_POLL_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Debug)]
enum XwininfoError {
    Spawn(io::Error),
    Wait(io::Error),
    Read(io::Error),
    MissingStdout,
    NonSuccess,
    TimedOut,
    OutputTooLarge,
}

impl std::fmt::Display for XwininfoError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(error) => write!(formatter, "could not start xwininfo: {error}"),
            Self::Wait(error) => write!(formatter, "could not wait for xwininfo: {error}"),
            Self::Read(error) => write!(formatter, "could not read xwininfo output: {error}"),
            Self::MissingStdout => formatter.write_str("xwininfo stdout pipe was not available"),
            Self::NonSuccess => formatter.write_str("xwininfo exited unsuccessfully"),
            Self::TimedOut => formatter.write_str("xwininfo timed out"),
            Self::OutputTooLarge => formatter.write_str("xwininfo output exceeded its limit"),
        }
    }
}

trait XwininfoCommandRunner: Send + Sync {
    fn run(&self, window_id: u64, timeout: Duration) -> Result<Vec<u8>, XwininfoError>;
}

struct SystemXwininfoCommandRunner;

impl XwininfoCommandRunner for SystemXwininfoCommandRunner {
    fn run(&self, window_id: u64, timeout: Duration) -> Result<Vec<u8>, XwininfoError> {
        let window_id = window_id.to_string();
        let mut command = Command::new("xwininfo");
        command
            .args(["-id", &window_id, "-stats"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        run_command_with_timeout(command, timeout)
    }
}

fn run_command_with_timeout(
    mut command: Command,
    timeout: Duration,
) -> Result<Vec<u8>, XwininfoError> {
    let mut child = command.spawn().map_err(XwininfoError::Spawn)?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                if let Err(error) = terminate_and_reap(&mut child) {
                    tracing::debug!("xwininfo timeout cleanup failed: {error}");
                }
                return Err(XwininfoError::TimedOut);
            }
            Ok(None) => thread::sleep(XWININFO_POLL_INTERVAL),
            Err(error) => {
                if let Err(cleanup_error) = terminate_and_reap(&mut child) {
                    tracing::debug!("xwininfo wait cleanup failed: {cleanup_error}");
                }
                return Err(XwininfoError::Wait(error));
            }
        }
    };
    if !status.success() {
        return Err(XwininfoError::NonSuccess);
    }

    let mut stdout = child.stdout.take().ok_or(XwininfoError::MissingStdout)?;
    let mut output = Vec::new();
    stdout
        .by_ref()
        .take((XWININFO_OUTPUT_LIMIT + 1) as u64)
        .read_to_end(&mut output)
        .map_err(XwininfoError::Read)?;
    if output.len() > XWININFO_OUTPUT_LIMIT {
        return Err(XwininfoError::OutputTooLarge);
    }
    Ok(output)
}

fn terminate_and_reap(child: &mut Child) -> io::Result<()> {
    let kill_result = child.kill();
    let wait_result = child.wait();
    match (kill_result, wait_result) {
        (_, Err(error)) => Err(error),
        (Err(error), Ok(_)) => Err(error),
        (Ok(()), Ok(_)) => Ok(()),
    }
}

/// Terminal metrics needed to reproduce original ueberzug's X11 conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalMetrics {
    columns: u16,
    rows: u16,
    pixel_width: u16,
    pixel_height: u16,
}

impl TerminalMetrics {
    fn read() -> Option<Self> {
        let tty = OpenOptions::new().read(true).open("/dev/tty").ok()?;
        let mut size = libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let result = unsafe { libc::ioctl(tty.as_raw_fd(), libc::TIOCGWINSZ, &mut size) };
        (result == 0).then_some(Self {
            columns: size.ws_col,
            rows: size.ws_row,
            pixel_width: size.ws_xpixel,
            pixel_height: size.ws_ypixel,
        })
    }
}

/// Coordinate correction for original ueberzug's shared padding model.
///
/// Original ueberzug multiplies cell coordinates by its inferred font size
/// and adds padding. Its normal path uses the larger horizontal or vertical
/// padding value for both axes, which can shift an otherwise correct terminal
/// cell rectangle. This calibration derives the correction from the current
/// PTY and X11 window metrics instead of using a resolution-specific offset.
#[derive(Debug, Clone, Copy, PartialEq)]
struct CoordinateCalibration {
    x_scale: f64,
    y_scale: f64,
    x_offset: f64,
    y_offset: f64,
}

impl CoordinateCalibration {
    fn discover() -> Option<(TerminalMetrics, Self)> {
        let metrics = TerminalMetrics::read()?;
        Self::discover_with_runner(
            metrics,
            std::env::var("WINDOWID").ok().as_deref(),
            &SystemXwininfoCommandRunner,
        )
    }

    fn discover_with_runner<R: XwininfoCommandRunner + ?Sized>(
        metrics: TerminalMetrics,
        window_id_value: Option<&str>,
        runner: &R,
    ) -> Option<(TerminalMetrics, Self)> {
        let window_id = parse_window_id(window_id_value)?;
        let (window_width, window_height) = query_window_size_with_runner(window_id, runner)?;
        let calibration = Self::from_metrics(metrics, window_width, window_height)?;
        Some((metrics, calibration))
    }

    fn from_metrics(
        metrics: TerminalMetrics,
        window_width: u32,
        window_height: u32,
    ) -> Option<Self> {
        if metrics.columns == 0 || metrics.rows == 0 || window_width == 0 || window_height == 0 {
            return None;
        }

        let columns = f64::from(metrics.columns);
        let rows = f64::from(metrics.rows);
        let pixel_width_u32 = if metrics.pixel_width == 0 {
            window_width
        } else {
            u32::from(metrics.pixel_width)
        };
        let pixel_height_u32 = if metrics.pixel_height == 0 {
            window_height
        } else {
            u32::from(metrics.pixel_height)
        };
        let pixel_width = f64::from(pixel_width_u32);
        let pixel_height = f64::from(pixel_height_u32);
        let mut padding_horizontal = guess_padding(columns, pixel_width);
        let mut padding_vertical = guess_padding(rows, pixel_height);
        let grid_font_width = (pixel_width - 2.0 * padding_horizontal) / columns;
        let grid_font_height = (pixel_height - 2.0 * padding_vertical) / rows;

        let uses_window_fallback =
            pixel_width < f64::from(window_width) && pixel_height < f64::from(window_height);
        if uses_window_fallback {
            padding_horizontal = f64::from(window_width - pixel_width_u32) / 2.0;
            padding_vertical = f64::from(window_height - pixel_height_u32) / 2.0;
        }

        if !grid_font_width.is_finite()
            || !grid_font_height.is_finite()
            || grid_font_width <= 0.0
            || grid_font_height <= 0.0
        {
            return None;
        }

        if uses_window_fallback {
            return Some(Self {
                x_scale: 1.0,
                y_scale: 1.0,
                x_offset: 0.0,
                y_offset: 0.0,
            });
        }

        let shared_padding = padding_horizontal.max(padding_vertical);
        let ueberzug_font_width = (pixel_width - 2.0 * shared_padding) / columns;
        let ueberzug_font_height = (pixel_height - 2.0 * shared_padding) / rows;
        if !ueberzug_font_width.is_finite()
            || !ueberzug_font_height.is_finite()
            || ueberzug_font_width <= 0.0
            || ueberzug_font_height <= 0.0
        {
            return None;
        }
        Some(Self {
            x_scale: grid_font_width / ueberzug_font_width,
            y_scale: grid_font_height / ueberzug_font_height,
            x_offset: (padding_horizontal - shared_padding) / ueberzug_font_width,
            y_offset: (padding_vertical - shared_padding) / ueberzug_font_height,
        })
    }

    fn transform(&self, overlay: &ArtworkOverlay) -> ArtworkOverlay {
        let mut calibrated = overlay.clone();
        calibrated.rect.x = transform_cell(overlay.rect.x, self.x_scale, self.x_offset);
        calibrated.rect.y = transform_cell(overlay.rect.y, self.y_scale, self.y_offset);
        calibrated.rect.width = transform_size(overlay.rect.width, self.x_scale);
        calibrated.rect.height = transform_size(overlay.rect.height, self.y_scale);
        calibrated
    }
}

fn guess_padding(cells: f64, pixels: f64) -> f64 {
    let font_size = (pixels / cells).floor();
    (-font_size * cells + pixels) / 2.0
}

fn transform_cell(cell: u16, scale: f64, offset: f64) -> u16 {
    (f64::from(cell) * scale + offset)
        .round()
        .max(0.0)
        .min(f64::from(u16::MAX)) as u16
}

fn transform_size(size: u16, scale: f64) -> u16 {
    (f64::from(size) * scale)
        .round()
        .max(if size == 0 { 0.0 } else { 1.0 })
        .min(f64::from(u16::MAX)) as u16
}

fn parse_window_id(value: Option<&str>) -> Option<u64> {
    value?.parse::<u64>().ok()
}

fn query_window_size_with_runner<R: XwininfoCommandRunner + ?Sized>(
    window_id: u64,
    runner: &R,
) -> Option<(u32, u32)> {
    let output = runner
        .run(window_id, XWININFO_TIMEOUT)
        .map_err(|error| {
            tracing::debug!(window_id, "xwininfo calibration probe failed: {error}");
        })
        .ok()?;
    let text = std::str::from_utf8(&output).ok().or_else(|| {
        tracing::debug!(window_id, "xwininfo returned non-UTF-8 output");
        None
    })?;
    let width = parse_xwininfo_dimension(text, "Width:")?;
    let height = parse_xwininfo_dimension(text, "Height:")?;
    Some((width, height))
}

fn parse_xwininfo_dimension(text: &str, label: &str) -> Option<u32> {
    let value = text
        .lines()
        .find_map(|line| line.trim_start().strip_prefix(label))?
        .trim()
        .parse::<u32>()
        .ok()?;
    (value > 0).then_some(value)
}

/// Owns the optional original ueberzug process and its active image.
pub struct Manager {
    writer: Option<Box<dyn Write + Send>>,
    process: Option<Arc<ExternalProcess>>,
    active: Option<ArtworkOverlay>,
    enabled: bool,
    calibration: Option<CoordinateCalibration>,
    refresh_calibration: bool,
    metrics: Option<TerminalMetrics>,
    calibration_refresh: Option<CalibrationRefresh>,
}

struct CalibrationRefresh {
    metrics: TerminalMetrics,
    result: Receiver<Option<(TerminalMetrics, CoordinateCalibration)>>,
}

impl Manager {
    /// Start the optional original ueberzug JSON layer.
    pub fn spawn() -> Option<Self> {
        let Some((metrics, calibration)) = CoordinateCalibration::discover() else {
            tracing::debug!("ueberzug calibration unavailable");
            return None;
        };
        let mut child = match command().spawn() {
            Ok(child) => child,
            Err(error) => {
                tracing::debug!("ueberzug unavailable: {error}");
                return None;
            }
        };
        let Some(stdin) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            tracing::debug!("ueberzug started without an IPC pipe");
            return None;
        };
        Some(Self::from_writer_and_child(
            Box::new(stdin),
            child,
            metrics,
            calibration,
        ))
    }

    /// Construct a manager around injected IO for deterministic unit tests.
    pub fn from_writer(writer: Box<dyn Write + Send>) -> Self {
        Self {
            writer: Some(writer),
            process: None,
            active: None,
            enabled: true,
            calibration: None,
            refresh_calibration: false,
            metrics: None,
            calibration_refresh: None,
        }
    }

    /// Construct a manager with deterministic calibration for unit tests.
    #[cfg(test)]
    fn from_writer_with_calibration(
        writer: Box<dyn Write + Send>,
        calibration: CoordinateCalibration,
    ) -> Self {
        Self {
            writer: Some(writer),
            process: None,
            active: None,
            enabled: true,
            calibration: Some(calibration),
            refresh_calibration: false,
            metrics: None,
            calibration_refresh: None,
        }
    }

    /// Whether the process and IPC path can still accept commands.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn from_writer_and_child(
        writer: Box<dyn Write + Send>,
        child: std::process::Child,
        metrics: TerminalMetrics,
        calibration: CoordinateCalibration,
    ) -> Self {
        Self {
            writer: Some(writer),
            process: Some(ExternalProcess::new(child)),
            active: None,
            enabled: true,
            calibration: Some(calibration),
            refresh_calibration: true,
            metrics: Some(metrics),
            calibration_refresh: None,
        }
    }

    /// Reconcile the external layer after ratatui has completed its frame.
    pub fn reconcile(&mut self, desired: Option<ArtworkOverlay>) {
        if !self.enabled {
            return;
        }
        if !self.refresh_runtime_calibration() {
            self.disable();
            return;
        }
        if let Some(process) = self.process.as_ref() {
            match process.has_exited() {
                Ok(true) | Err(_) => {
                    self.disable();
                    return;
                }
                Ok(false) => {}
            }
        }
        if self.active.as_ref() == desired.as_ref() {
            return;
        }
        if let Some(previous) = self.active.take()
            && self.send(remove_command(&previous.identifier)).is_err()
        {
            return;
        }
        if let Some(next) = desired
            && self
                .send(add_command_with_calibration(&next, self.calibration))
                .is_ok()
        {
            self.active = Some(next);
        }
    }

    fn send(&mut self, command: Value) -> io::Result<()> {
        if let Some(process) = self.process.as_ref()
            && process.has_exited()?
        {
            self.disable();
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "ueberzug exited"));
        }
        let Some(writer) = self.writer.as_mut() else {
            self.disable();
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ueberzug IPC unavailable",
            ));
        };
        let result = (|| {
            serde_json::to_writer(&mut **writer, &command)?;
            writer.write_all(b"\n")?;
            writer.flush()
        })();
        if result.is_err() {
            self.disable();
        }
        result
    }

    fn disable(&mut self) {
        self.enabled = false;
        self.writer.take();
        if let Some(process) = self.process.take() {
            process.terminate();
        }
    }

    fn refresh_runtime_calibration(&mut self) -> bool {
        if !self.refresh_calibration {
            return true;
        }
        let Some(metrics) = TerminalMetrics::read() else {
            return false;
        };

        let pending_result = self
            .calibration_refresh
            .as_ref()
            .map(|pending| (pending.metrics, pending.result.try_recv()));
        if let Some((pending_metrics, result)) = pending_result {
            match result {
                Ok(Some((new_metrics, calibration)))
                    if new_metrics == pending_metrics && new_metrics == metrics =>
                {
                    self.calibration_refresh = None;
                    self.metrics = Some(new_metrics);
                    self.calibration = Some(calibration);
                }
                Ok(Some(_)) => {
                    self.calibration_refresh = None;
                }
                Ok(None) | Err(TryRecvError::Disconnected) => return false,
                Err(TryRecvError::Empty) => return true,
            }
        }
        if self.metrics == Some(metrics) {
            return self.calibration.is_some();
        }

        if self.calibration_refresh.is_none() {
            let (sender, receiver) = mpsc::channel();
            thread::spawn(move || {
                let result = CoordinateCalibration::discover_with_runner(
                    metrics,
                    std::env::var("WINDOWID").ok().as_deref(),
                    &SystemXwininfoCommandRunner,
                );
                let _ = sender.send(result);
            });
            self.calibration_refresh = Some(CalibrationRefresh {
                metrics,
                result: receiver,
            });
        };
        true
    }

    fn cleanup_layer(&mut self) {
        if let Some(active) = self.active.take() {
            let _ = self.send(remove_command(&active.identifier));
        }
        self.disable();
    }
}

impl ExternalArtworkLayer for Manager {
    fn reconcile(&mut self, desired: Option<ArtworkOverlay>) {
        Self::reconcile(self, desired);
    }

    fn is_healthy(&self) -> bool {
        self.is_enabled()
    }

    fn cleanup(&mut self) {
        self.cleanup_layer();
    }

    fn panic_cleanup(&self) -> Option<Arc<ExternalProcess>> {
        self.process.as_ref().map(Arc::clone)
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        self.cleanup_layer();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("capture lock")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct ScriptedXwininfo {
        response: Mutex<Option<Result<Vec<u8>, XwininfoError>>>,
        calls: Arc<Mutex<Vec<(u64, Duration)>>>,
    }

    impl ScriptedXwininfo {
        fn new(
            response: Result<Vec<u8>, XwininfoError>,
            calls: Arc<Mutex<Vec<(u64, Duration)>>>,
        ) -> Self {
            Self {
                response: Mutex::new(Some(response)),
                calls,
            }
        }
    }

    impl XwininfoCommandRunner for ScriptedXwininfo {
        fn run(&self, window_id: u64, timeout: Duration) -> Result<Vec<u8>, XwininfoError> {
            self.calls
                .lock()
                .expect("xwininfo call lock")
                .push((window_id, timeout));
            self.response
                .lock()
                .expect("xwininfo response lock")
                .take()
                .expect("scripted xwininfo called once")
        }
    }

    fn overlay() -> ArtworkOverlay {
        ArtworkOverlay::new(
            4,
            "/cache/cover.png".into(),
            Rect {
                x: 7,
                y: 3,
                width: 20,
                height: 10,
            },
        )
    }

    #[test]
    fn command_uses_original_json_parser_mode() {
        let command = command();
        assert_eq!(command.get_program(), "ueberzug");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["layer", "--parser", "json"]
        );
    }

    #[test]
    fn legacy_payload_uses_legacy_scaler() {
        assert_eq!(add_command(&overlay())["scaler"], "contain");
        assert_eq!(remove_command("art")["action"], "remove");
    }

    #[test]
    fn xwininfo_success_uses_the_bounded_injected_runner() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner =
            ScriptedXwininfo::new(Ok(b"Width: 800\nHeight: 480\n".to_vec()), calls.clone());

        assert_eq!(query_window_size_with_runner(42, &runner), Some((800, 480)));
        assert_eq!(
            *calls.lock().expect("xwininfo call lock"),
            vec![(42, XWININFO_TIMEOUT)]
        );
    }

    #[test]
    fn malformed_xwininfo_output_falls_back_without_calibration() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner =
            ScriptedXwininfo::new(Ok(b"Width: not-a-number\nHeight: 480\n".to_vec()), calls);
        let metrics = TerminalMetrics {
            columns: 80,
            rows: 24,
            pixel_width: 800,
            pixel_height: 480,
        };

        assert_eq!(
            CoordinateCalibration::discover_with_runner(metrics, Some("42"), &runner),
            None
        );
    }

    #[test]
    fn timed_out_xwininfo_falls_back_without_calibration() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner = ScriptedXwininfo::new(Err(XwininfoError::TimedOut), calls);
        let metrics = TerminalMetrics {
            columns: 80,
            rows: 24,
            pixel_width: 800,
            pixel_height: 480,
        };

        assert_eq!(
            CoordinateCalibration::discover_with_runner(metrics, Some("42"), &runner),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn timed_out_command_is_terminated_and_reaped() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30"]);

        let result = run_command_with_timeout(command, Duration::from_millis(10));

        assert!(matches!(result, Err(XwininfoError::TimedOut)));
    }

    #[test]
    fn calibration_uses_window_dimensions_when_pty_pixel_metrics_are_zero() {
        let metrics = TerminalMetrics {
            columns: 80,
            rows: 24,
            pixel_width: 0,
            pixel_height: 0,
        };
        let calibration =
            CoordinateCalibration::from_metrics(metrics, 800, 480).expect("valid X11 fallback");
        assert_eq!(calibration.x_scale, 1.0);
        assert_eq!(calibration.y_scale, 1.0);
        assert_eq!(calibration.x_offset, 0.0);
        assert_eq!(calibration.y_offset, 0.0);
    }

    #[test]
    fn calibration_rejects_missing_or_zero_required_metrics() {
        let valid_window = (800, 480);
        assert_eq!(
            CoordinateCalibration::from_metrics(
                TerminalMetrics {
                    columns: 0,
                    rows: 24,
                    pixel_width: 0,
                    pixel_height: 0,
                },
                valid_window.0,
                valid_window.1,
            ),
            None
        );
        assert_eq!(
            CoordinateCalibration::from_metrics(
                TerminalMetrics {
                    columns: 80,
                    rows: 0,
                    pixel_width: 0,
                    pixel_height: 0,
                },
                valid_window.0,
                valid_window.1,
            ),
            None
        );
        assert_eq!(
            CoordinateCalibration::from_metrics(
                TerminalMetrics {
                    columns: 80,
                    rows: 24,
                    pixel_width: 0,
                    pixel_height: 0,
                },
                0,
                valid_window.1,
            ),
            None
        );
        assert_eq!(parse_window_id(None), None);
        assert_eq!(parse_window_id(Some("not-a-window")), None);
        assert_eq!(parse_xwininfo_dimension("Width: invalid", "Width:"), None);
        assert_eq!(parse_xwininfo_dimension("Width: 0", "Width:"), None);
    }

    #[test]
    fn calibration_matches_ueberzug_padding_model_without_fixed_offsets() {
        let metrics = TerminalMetrics {
            columns: 80,
            rows: 24,
            pixel_width: 800,
            pixel_height: 935,
        };
        let calibration =
            CoordinateCalibration::from_metrics(metrics, 800, 935).expect("valid metrics");

        assert!(calibration.x_scale > 1.0);
        assert_eq!(calibration.y_scale, 1.0);
        assert!(calibration.x_offset < -1.0);
        assert!(calibration.y_offset.abs() < f64::EPSILON);

        let fallback =
            CoordinateCalibration::from_metrics(metrics, 900, 1000).expect("valid metrics");
        assert_eq!(fallback.x_scale, 1.0);
        assert_eq!(fallback.y_scale, 1.0);
        assert_eq!(fallback.x_offset, 0.0);
        assert_eq!(fallback.y_offset, 0.0);
    }

    #[test]
    fn calibrated_payload_transforms_only_original_ueberzug_coordinates() {
        let calibration = CoordinateCalibration {
            x_scale: 1.0,
            y_scale: 1.0,
            x_offset: -1.25,
            y_offset: 0.75,
        };
        let value = add_command_with_calibration(&overlay(), Some(calibration));

        assert_eq!(value["x"], 6);
        assert_eq!(value["y"], 4);
        assert_eq!(value["width"], 20);
        assert_eq!(value["height"], 10);
        assert_eq!(value["scaler"], "contain");
    }

    #[test]
    fn calibrated_manager_writes_transformed_payload() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let calibration = CoordinateCalibration {
            x_scale: 1.0,
            y_scale: 1.0,
            x_offset: -1.0,
            y_offset: 1.0,
        };
        let mut manager =
            Manager::from_writer_with_calibration(Box::new(Capture(bytes.clone())), calibration);
        manager.reconcile(Some(overlay()));

        let text =
            String::from_utf8(bytes.lock().expect("capture lock").clone()).expect("JSON payload");
        assert!(text.contains("\"x\":6"));
        assert!(text.contains("\"y\":4"));
    }

    #[test]
    fn broken_process_io_disables_without_panicking() {
        struct Broken;

        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut manager = Manager::from_writer(Box::new(Broken));
        manager.reconcile(Some(overlay()));
        assert!(!manager.is_enabled());
    }
}
