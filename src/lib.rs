//! Cross-platform native app automation for Rust.
//!
//! Launch, focus, control, and capture output from GUI applications programmatically.
//! The API mirrors `std::process::Command`/`Child` for familiarity, with additional
//! GUI-specific methods for window focus, keystroke injection, and background launching.
//!
//! # Example
//!
//! ```no_run
//! use launch_control::Command;
//! use std::process::Stdio;
//!
//! // Simple launch (like std::process::Command)
//! let mut child = Command::new("/Applications/Safari.app")
//!     .arg("https://example.com")
//!     .spawn()?;
//!
//! // With output capture
//! let mut child = Command::new("/Applications/Foo.app")
//!     .stdout(Stdio::piped())
//!     .stderr(Stdio::piped())
//!     .background(true)
//!     .spawn()?;
//!
//! // GUI-specific extras
//! child.focus()?;
//! child.kill()?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod error;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

pub use error::{Error, Result};
pub use keyboard_types::{Code, Modifiers};

use std::ffi::OsStr;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{mpsc, Arc, Mutex};

/// Internal fire-once state for exit notification.
/// Transitions from `Pending(callbacks)` → `Exited(status)` exactly once.
enum ExitState {
    Pending(Vec<Box<dyn FnOnce(ExitStatus) + Send>>),
    Exited(ExitStatus),
}

impl Default for ExitState {
    fn default() -> Self {
        Self::Pending(Vec::new())
    }
}

pub(crate) type ExitStateShared = Arc<Mutex<ExitState>>;

/// Start the OS-native exit watcher for a PID. When the process exits, fire
/// all registered callbacks. Delegates to `parent_exit::on_pid_exit` which
/// uses kqueue on macOS/BSD, waitpid on Linux, and WaitForSingleObject on
/// Windows — single source of truth for process-exit primitives.
///
/// We can't read the real exit code of non-child processes on macOS (NSWorkspace
/// launches are reparented to launchd), so the status passed to callbacks is a
/// synthesized successful exit. Callers that need the real exit code should use
/// `Child::try_wait()` / `wait()` instead.
pub(crate) fn start_exit_watcher(pid: u32, state: ExitStateShared) {
    parent_exit::on_pid_exit(pid, move || {
        let status = synth_exit_status();
        let mut guard = state.lock().unwrap();
        let old = std::mem::replace(&mut *guard, ExitState::Exited(status));
        drop(guard);
        if let ExitState::Pending(callbacks) = old {
            for cb in callbacks {
                cb(status);
            }
        }
    });
}

#[cfg(unix)]
fn synth_exit_status() -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    ExitStatus::from_raw(0)
}

#[cfg(windows)]
fn synth_exit_status() -> ExitStatus {
    use std::os::windows::process::ExitStatusExt;
    ExitStatus::from_raw(0)
}

/// Builder for launching an application. Mirrors `std::process::Command`.
///
/// On macOS, the path should be an `.app` bundle (e.g. `/Applications/Safari.app`).
/// A binary path inside `.app/Contents/MacOS/` is also accepted and will be resolved
/// to the bundle automatically.
///
/// On Windows, the path should be the executable path.
pub struct Command {
    pub(crate) path: PathBuf,
    pub(crate) args: Vec<String>,
    pub(crate) background: bool,
    /// URL to open via the app (e.g. custom scheme URLs).
    /// When set, uses NSWorkspace.openURLs on macOS instead of openApplicationAtURL.
    pub(crate) url: Option<String>,
    pub(crate) stdout_cfg: Option<Stdio>,
    pub(crate) stderr_cfg: Option<Stdio>,
}

impl Command {
    /// Create a new command for the application at the given path.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            args: Vec::new(),
            background: false,
            url: None,
            stdout_cfg: None,
            stderr_cfg: None,
        }
    }

    /// Add a single argument.
    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_string_lossy().to_string());
        self
    }

    /// Add multiple arguments.
    pub fn args(&mut self, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> &mut Self {
        for a in args {
            self.args.push(a.as_ref().to_string_lossy().to_string());
        }
        self
    }

    /// Open a URL via this application (e.g. custom scheme or `https://` URLs).
    /// On macOS, uses `NSWorkspace.openURLs` to route the URL through the app.
    pub fn url(&mut self, url: impl Into<String>) -> &mut Self {
        self.url = Some(url.into());
        self
    }

    /// If true, launch without stealing focus (background mode).
    /// Default: false.
    pub fn background(&mut self, bg: bool) -> &mut Self {
        self.background = bg;
        self
    }

    /// Configure stdout handling. Use `Stdio::piped()` to capture output.
    pub fn stdout(&mut self, cfg: Stdio) -> &mut Self {
        self.stdout_cfg = Some(cfg);
        self
    }

    /// Configure stderr handling. Use `Stdio::piped()` to capture output.
    pub fn stderr(&mut self, cfg: Stdio) -> &mut Self {
        self.stderr_cfg = Some(cfg);
        self
    }

    /// Spawn the application and return a handle for lifecycle management.
    pub fn spawn(&mut self) -> Result<Child> {
        platform_spawn(self)
    }
}

/// Handle to a running application process. Mirrors `std::process::Child`.
///
/// GUI-specific methods (`focus`, `send_save_keystroke`) are available in addition
/// to the standard process lifecycle methods.
pub struct Child {
    pid: u32,
    /// Captured stdout (only present when launched with `.stdout(Stdio::piped())`).
    pub stdout: Option<ChildStdout>,
    /// Captured stderr (only present when launched with `.stderr(Stdio::piped())`).
    pub stderr: Option<ChildStderr>,
    /// Shared exit state for `on_exit` callbacks. Transitions once.
    pub(crate) exit_state: ExitStateShared,
    #[cfg(target_os = "macos")]
    inner: Option<macos::MacOSHandle>,
    #[cfg(target_os = "windows")]
    inner: windows::WindowsHandle,
}

impl Child {
    /// The OS process identifier. Mirrors `std::process::Child::id()`.
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Force-terminate the application. Mirrors `std::process::Child::kill()`.
    pub fn kill(&mut self) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            let pid = self.pid;
            let ret = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            if ret != 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::ESRCH) {
                    return Ok(()); // already dead
                }
                return Err(err);
            }
            return Ok(());
        }
        #[cfg(target_os = "windows")]
        {
            return self.inner.kill_io();
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Err(io::Error::new(io::ErrorKind::Unsupported, "unsupported platform"))
        }
    }

    /// Wait for the process to exit. Mirrors `std::process::Child::wait()`.
    ///
    /// On macOS, this polls process liveness since GUI apps launched via NSWorkspace
    /// are not direct children (so `waitpid` is not available).
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    /// Check if the process has exited without blocking. Mirrors `std::process::Child::try_wait()`.
    ///
    /// Returns `Ok(Some(status))` if exited, `Ok(None)` if still running.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        #[cfg(target_os = "macos")]
        {
            return macos::try_wait(self.pid);
        }
        #[cfg(target_os = "windows")]
        {
            return self.inner.try_wait();
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Err(io::Error::new(io::ErrorKind::Unsupported, "unsupported platform"))
        }
    }

    // -- Exit observation (multi-observer, event-driven, no polling) --

    /// Register a callback that fires when the process exits.
    ///
    /// Event-driven (OS-native primitives — kqueue on macOS, WaitForSingleObject
    /// on Windows). Does not require `&mut self` — multiple callbacks may be
    /// registered from independent code paths, and none interferes with `kill()`
    /// or ownership of the `Child` handle.
    ///
    /// If the process has already exited, the callback fires immediately on the
    /// calling thread. Otherwise it fires on a dedicated watcher thread.
    ///
    /// Callback receives the process's `ExitStatus`. On macOS, apps launched via
    /// NSWorkspace are not direct children, so we cannot read a real exit code;
    /// the status will be a synthesized successful exit. On Windows, the real
    /// `GetExitCodeProcess` value is reported.
    pub fn on_exit(&self, callback: impl FnOnce(ExitStatus) + Send + 'static) {
        let mut state = self.exit_state.lock().unwrap();
        match &mut *state {
            ExitState::Pending(cbs) => cbs.push(Box::new(callback)),
            ExitState::Exited(status) => {
                let status = *status;
                drop(state);
                callback(status);
            }
        }
    }

    // -- GUI-specific extras (not on std::process::Child) --

    /// Bring the application to the foreground.
    pub fn focus(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            return match self.inner {
                Some(ref inner) => inner.focus(),
                None => Err(Error::Platform("no GUI handle available".into())),
            };
        }
        #[cfg(target_os = "windows")]
        {
            return self.inner.focus();
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Err(Error::Unsupported)
        }
    }

    /// Send a keystroke to the process. Does not require the app to be focused.
    ///
    /// Uses `CGEventPostToPSN` on macOS to post directly to the process's event queue.
    pub fn send_keystroke(&self, code: Code, modifiers: Modifiers) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            return match self.inner {
                Some(ref inner) => inner.send_keystroke(code, modifiers),
                None => Err(Error::Platform("no GUI handle available".into())),
            };
        }
        #[cfg(target_os = "windows")]
        {
            // TODO: Use SendMessage/PostMessage for PID-targeted keystroke on Windows
            let _ = (code, modifiers);
            return Err(Error::Unsupported);
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = (code, modifiers);
            Err(Error::Unsupported)
        }
    }
}

/// Captured stdout from a launched application. Implements `Read` and `BufRead`.
///
/// Backed by a background drain thread that reads eagerly from the underlying
/// PTY/pipe to prevent the application from blocking when buffers fill.
/// If this value is dropped, the drain thread continues consuming data.
pub struct ChildStdout {
    rx: mpsc::Receiver<String>,
    buf: Vec<u8>,
    pos: usize,
}

impl ChildStdout {
    /// Receive a single line with a timeout. Convenience for callers that need
    /// deadline-based reading (e.g. waiting for startup markers).
    pub fn recv_timeout(&self, timeout: std::time::Duration) -> std::result::Result<String, mpsc::RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }
}

impl Read for ChildStdout {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.buf.len() {
            match self.rx.recv() {
                Ok(line) => {
                    self.buf = format!("{line}\n").into_bytes();
                    self.pos = 0;
                }
                Err(_) => return Ok(0), // EOF — drain thread exited
            }
        }
        let n = std::cmp::min(buf.len(), self.buf.len() - self.pos);
        buf[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl BufRead for ChildStdout {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.pos >= self.buf.len() {
            match self.rx.recv() {
                Ok(line) => {
                    self.buf = format!("{line}\n").into_bytes();
                    self.pos = 0;
                }
                Err(_) => {
                    self.buf.clear();
                    self.pos = 0;
                    return Ok(&[]);
                }
            }
        }
        Ok(&self.buf[self.pos..])
    }

    fn consume(&mut self, amt: usize) {
        self.pos += amt;
    }
}

/// Captured stderr from a launched application. Implements `Read` and `BufRead`.
///
/// Identical to `ChildStdout` — separate type for API clarity.
pub struct ChildStderr {
    rx: mpsc::Receiver<String>,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for ChildStderr {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.buf.len() {
            match self.rx.recv() {
                Ok(line) => {
                    self.buf = format!("{line}\n").into_bytes();
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = std::cmp::min(buf.len(), self.buf.len() - self.pos);
        buf[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl BufRead for ChildStderr {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.pos >= self.buf.len() {
            match self.rx.recv() {
                Ok(line) => {
                    self.buf = format!("{line}\n").into_bytes();
                    self.pos = 0;
                }
                Err(_) => {
                    self.buf.clear();
                    self.pos = 0;
                    return Ok(&[]);
                }
            }
        }
        Ok(&self.buf[self.pos..])
    }

    fn consume(&mut self, amt: usize) {
        self.pos += amt;
    }
}

/// Start a background thread that reads lines and sends to channel.
/// If receiver is dropped, continues draining to prevent pipe blocking.
pub(crate) fn start_drain_thread(reader: impl Read + Send + 'static, tx: mpsc::Sender<String>) {
    std::thread::spawn(move || {
        let reader = BufReader::new(reader);
        for line in reader.lines() {
            match line {
                Ok(l) => { let _ = tx.send(l); }
                Err(_) => break,
            }
        }
    });
}

pub(crate) fn make_child_stdout(rx: mpsc::Receiver<String>) -> ChildStdout {
    ChildStdout { rx, buf: Vec::new(), pos: 0 }
}

pub(crate) fn make_child_stderr(rx: mpsc::Receiver<String>) -> ChildStderr {
    ChildStderr { rx, buf: Vec::new(), pos: 0 }
}

// -- Platform dispatch --

#[cfg(target_os = "macos")]
fn platform_spawn(cmd: &mut Command) -> Result<Child> {
    macos::spawn(cmd)
}

#[cfg(target_os = "windows")]
fn platform_spawn(cmd: &mut Command) -> Result<Child> {
    windows::spawn(cmd)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn platform_spawn(_cmd: &mut Command) -> Result<Child> {
    Err(Error::Unsupported)
}
