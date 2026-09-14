//! Optional ueberzugpp image-layer integration.
//!
//! The adapter is deliberately independent from ratatui-image. Ratatui still
//! paints every frame first, and this process is only an additional layer when
//! automatic terminal detection selected half-blocks.

use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;

pub use super::backend::ArtworkOverlay;
use super::backend::{ExternalArtworkLayer, ExternalProcess};
use serde_json::{Value, json};

/// Build the ueberzugpp layer invocation.
pub(crate) fn command() -> Command {
    let mut command = Command::new("ueberzugpp");
    command
        .arg("layer")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// Build an ueberzugpp `add` JSON object from backend-neutral artwork data.
pub fn add_command(overlay: &ArtworkOverlay) -> Value {
    json!({
        "action": "add",
        "identifier": overlay.identifier,
        "path": overlay.path,
        "x": overlay.rect.x,
        "y": overlay.rect.y,
        "width": overlay.rect.width,
        "height": overlay.rect.height,
        "scaler": "fit_contain",
    })
}

/// Build an ueberzugpp `remove` JSON object.
pub fn remove_command(identifier: &str) -> Value {
    json!({
        "action": "remove",
        "identifier": identifier,
    })
}

/// Owns the optional ueberzugpp layer process and its active image.
pub struct Manager {
    writer: Option<Box<dyn Write + Send>>,
    process: Option<Arc<ExternalProcess>>,
    active: Option<ArtworkOverlay>,
    enabled: bool,
}

impl Manager {
    /// Start `ueberzugpp layer` without waiting on executable probing or a
    /// backend negotiation. A failed spawn simply leaves native half-blocks in
    /// charge.
    pub fn spawn() -> Option<Self> {
        let mut child = match command().spawn() {
            Ok(child) => child,
            Err(error) => {
                tracing::debug!("ueberzugpp unavailable: {error}");
                return None;
            }
        };
        let Some(stdin) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            tracing::debug!("ueberzugpp started without an IPC pipe");
            return None;
        };
        Some(Self::from_writer_and_child(Box::new(stdin), child))
    }

    /// Construct a manager around injected IO for deterministic unit tests.
    pub fn from_writer(writer: Box<dyn Write + Send>) -> Self {
        Self {
            writer: Some(writer),
            process: None,
            active: None,
            enabled: true,
        }
    }

    /// Whether the process and IPC path can still accept commands.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn from_writer_and_child(writer: Box<dyn Write + Send>, child: std::process::Child) -> Self {
        Self {
            writer: Some(writer),
            process: Some(ExternalProcess::new(child)),
            active: None,
            enabled: true,
        }
    }

    /// Reconcile the external layer after ratatui has completed its frame.
    ///
    /// Replacement always removes the old identifier before adding the new
    /// one. Any process or pipe failure disables this optional path and leaves
    /// the already-rendered native fallback untouched.
    pub fn reconcile(&mut self, desired: Option<ArtworkOverlay>) {
        if !self.enabled {
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
            && self.send(add_command(&next)).is_ok()
        {
            self.active = Some(next);
        }
    }

    fn send(&mut self, command: Value) -> io::Result<()> {
        if let Some(process) = self.process.as_ref()
            && process.has_exited()?
        {
            self.disable();
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ueberzugpp exited",
            ));
        }
        let Some(writer) = self.writer.as_mut() else {
            self.disable();
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ueberzugpp IPC unavailable",
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
    use std::path::PathBuf;
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

    fn overlay(path: &str) -> ArtworkOverlay {
        ArtworkOverlay::new(
            4,
            PathBuf::from(path),
            Rect {
                x: 7,
                y: 3,
                width: 20,
                height: 10,
            },
        )
    }

    #[test]
    fn add_command_contains_terminal_geometry_and_path() {
        let value = add_command(&overlay("/cache/cover.png"));
        assert_eq!(value["action"], "add");
        assert_eq!(value["path"], "/cache/cover.png");
        assert_eq!(value["x"], 7);
        assert_eq!(value["y"], 3);
        assert_eq!(value["width"], 20);
        assert_eq!(value["height"], 10);
        assert_eq!(value["scaler"], "fit_contain");
    }

    #[test]
    fn command_uses_ueberzugpp_layer_mode() {
        let command = command();
        assert_eq!(command.get_program(), "ueberzugpp");
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["layer"]);
    }

    #[test]
    fn remove_command_targets_only_the_identifier() {
        let value = remove_command("harmonium-artwork-id");
        assert_eq!(
            value,
            json!({
                "action": "remove",
                "identifier": "harmonium-artwork-id",
            })
        );
    }

    #[test]
    fn replacement_removes_before_adding_and_same_overlay_is_not_replayed() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let mut manager = Manager::from_writer(Box::new(Capture(bytes.clone())));
        let first = overlay("/cache/one.png");
        let second = overlay("/cache/two.png");

        manager.reconcile(Some(first.clone()));
        manager.reconcile(Some(first));
        manager.reconcile(Some(second));

        let text = String::from_utf8(bytes.lock().expect("capture lock").clone()).unwrap();
        let remove = text.find("\"action\":\"remove\"").expect("remove command");
        let add = text[remove..]
            .find("\"action\":\"add\"")
            .expect("replacement add command");
        assert!(add > 0, "replacement add must follow remove");
        assert_eq!(text.matches("\"action\":\"add\"").count(), 2);
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
        manager.reconcile(Some(overlay("/cache/cover.png")));
        manager.reconcile(None);
    }
}
