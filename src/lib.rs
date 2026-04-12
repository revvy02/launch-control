mod error;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

pub use error::{Error, Result};

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::io::{BufRead, BufReader, Read};

/// Builder for launching an application.
pub struct AppLauncher {
    path: PathBuf,
    args: Vec<String>,
    background: bool,
    /// URL to open via the app (e.g. custom scheme URLs).
    /// When set, uses NSWorkspace.openURLs on macOS instead of openApplicationAtURL.
    url: Option<String>,
}

impl AppLauncher {
    /// Create a new launcher for the application at the given path.
    ///
    /// On macOS, this should be an `.app` bundle path (e.g. `/Applications/Safari.app`).
    /// A binary path inside `.app/Contents/MacOS/` is also accepted and will be resolved
    /// to the bundle automatically.
    ///
    /// On Windows, this should be the executable path.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            args: Vec::new(),
            background: false,
            url: None,
        }
    }

    /// Add a single argument.
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_string_lossy().to_string());
        self
    }

    /// Add multiple arguments.
    pub fn args(mut self, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Self {
        for a in args {
            self.args.push(a.as_ref().to_string_lossy().to_string());
        }
        self
    }

    /// Open a URL via this application (e.g. custom scheme or `https://` URLs).
    /// On macOS, uses `NSWorkspace.openURLs` to route the URL through the app.
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    /// If true, launch without stealing focus (background mode).
    /// Default: false.
    pub fn background(mut self, bg: bool) -> Self {
        self.background = bg;
        self
    }

    /// Spawn the application and return a handle for lifecycle management.
    pub fn spawn(self) -> Result<AppHandle> {
        platform_spawn(self)
    }

    /// Spawn the application via `Command::new()` with stderr piped, returning
    /// a handle that supports both stdio access and GUI control (focus/kill).
    ///
    /// On macOS, resolves the binary from the `.app` bundle and uses
    /// `NSRunningApplication` from the PID for focus/keystroke support.
    /// Spawn with piped stdout/stderr. Returns the handle and output channels separately.
    /// Output is auto-drained by background threads — dropping the channels is safe.
    pub fn spawn_piped(self) -> Result<(PipedAppHandle, PipedOutput)> {
        platform_spawn_piped(self)
    }
}

/// Handle to a running application process.
pub struct AppHandle {
    pid: u32,
    #[cfg(target_os = "macos")]
    inner: macos::MacOSHandle,
    #[cfg(target_os = "windows")]
    inner: windows::WindowsHandle,
}

impl AppHandle {
    /// The OS process identifier.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Check if the application is still running.
    pub fn is_running(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            return self.inner.is_running();
        }
        #[cfg(target_os = "windows")]
        {
            return self.inner.is_running();
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            false
        }
    }

    /// Bring the application to the foreground.
    pub fn focus(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            return self.inner.focus();
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

    /// Force-terminate the application.
    pub fn kill(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            return self.inner.kill();
        }
        #[cfg(target_os = "windows")]
        {
            return self.inner.kill();
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Err(Error::Unsupported)
        }
    }

    /// Send Cmd+S (macOS) or Ctrl+S (Windows) directly to the process.
    /// Does not require the app to be focused.
    pub fn send_save_keystroke(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            return self.inner.send_cmd_s();
        }
        #[cfg(target_os = "windows")]
        {
            // TODO: Use SendMessage/PostMessage for PID-targeted keystroke on Windows
            return Err(Error::Unsupported);
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Err(Error::Unsupported)
        }
    }
}

/// Output channels from a piped app launch.
///
/// Drain threads run automatically in the background. If the receiver is dropped,
/// the drain thread continues reading (preventing pipe blocking) but discards lines.
pub struct PipedOutput {
    pub stdout: Option<mpsc::Receiver<String>>,
    pub stderr: Option<mpsc::Receiver<String>>,
}

/// Handle to a running application launched with piped stdout/stderr.
///
/// On macOS, uses `/usr/bin/open` with FIFOs for background launch + output capture.
/// GUI operations (focus, kill, keystroke) use `NSRunningApplication`.
///
/// Stdout/stderr are drained by background threads automatically.
/// The output channels are returned separately via `spawn_piped()` so they
/// don't need to be stored in the handle (which may be shared across threads).
pub struct PipedAppHandle {
    pid: u32,
    #[cfg(target_os = "macos")]
    inner: Option<macos::MacOSHandle>,
}

/// Start a background thread that reads lines and sends to channel.
/// If receiver is dropped, continues draining to prevent pipe blocking.
fn start_drain_thread(reader: impl Read + Send + 'static, tx: mpsc::Sender<String>) {
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

impl PipedAppHandle {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn is_running(&self) -> bool {
        #[cfg(target_os = "macos")]
        if let Some(ref inner) = self.inner {
            return inner.is_running();
        }
        unsafe { libc::kill(self.pid as i32, 0) == 0 }
    }

    pub fn focus(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        if let Some(ref inner) = self.inner {
            return inner.focus();
        }
        Err(Error::Unsupported)
    }

    pub fn kill(&self) -> Result<()> {
        let ret = unsafe { libc::kill(self.pid as i32, libc::SIGKILL) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ESRCH) {
                return Ok(());
            }
            return Err(Error::Platform(format!(
                "failed to kill pid {}: {err}",
                self.pid
            )));
        }
        Ok(())
    }

    pub fn send_save_keystroke(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        if let Some(ref inner) = self.inner {
            return inner.send_cmd_s();
        }
        Err(Error::Unsupported)
    }

}


#[cfg(target_os = "macos")]
fn platform_spawn(launcher: AppLauncher) -> Result<AppHandle> {
    macos::spawn(launcher)
}

#[cfg(target_os = "windows")]
fn platform_spawn(launcher: AppLauncher) -> Result<AppHandle> {
    windows::spawn(launcher)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn platform_spawn(_launcher: AppLauncher) -> Result<AppHandle> {
    Err(Error::Unsupported)
}

#[cfg(target_os = "macos")]
fn platform_spawn_piped(launcher: AppLauncher) -> Result<(PipedAppHandle, PipedOutput)> {
    macos::spawn_piped(launcher)
}

#[cfg(target_os = "windows")]
fn platform_spawn_piped(_launcher: AppLauncher) -> Result<(PipedAppHandle, PipedOutput)> {
    Err(Error::Unsupported)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn platform_spawn_piped(_launcher: AppLauncher) -> Result<(PipedAppHandle, PipedOutput)> {
    Err(Error::Unsupported)
}
