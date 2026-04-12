use crate::error::{Error, Result};
use crate::Command;

use objc2::rc::Retained;
use objc2_app_kit::{
    NSApplicationActivationOptions, NSRunningApplication, NSWorkspace,
    NSWorkspaceOpenConfiguration,
};
use objc2_foundation::{NSArray, NSString, NSURL};

use std::io;
use std::process::ExitStatus;
use std::sync::mpsc;

/// macOS-specific handle wrapping NSRunningApplication.
pub(crate) struct MacOSHandle {
    app: Retained<NSRunningApplication>,
    pid: u32,
}

impl MacOSHandle {
    pub fn focus(&self) -> Result<()> {
        if self.app.isTerminated() {
            return Err(Error::Terminated);
        }
        // Unhide first — activateFromApplication can fail on hidden apps.
        if self.app.isHidden() {
            self.app.unhide();
        }
        // Use activateFromApplication to bring the app to the foreground.
        // ActivateAllWindows brings all windows forward (not just main/key).
        let current = NSRunningApplication::currentApplication();
        // activateFromApplication return value is unreliable from CLI processes —
        // it often returns false even when activation succeeds.
        self.app.activateFromApplication_options(
            &current,
            NSApplicationActivationOptions::ActivateAllWindows,
        );
        Ok(())
    }

    /// Send a keystroke directly to this process without requiring focus.
    /// Uses CGEventPostToPSN to post keyboard events to the process's event queue.
    pub fn send_keystroke(&self, code: keyboard_types::Code, modifiers: keyboard_types::Modifiers) -> Result<()> {
        let keycode = code_to_macos_keycode(code)
            .ok_or_else(|| Error::Platform(format!("unsupported key: {code:?}")))?;
        let flags = modifiers_to_cg_flags(modifiers);
        send_keystroke_to_pid(self.pid, keycode, flags)
    }
}

// CoreGraphics / Carbon FFI for CGEventPostToPSN
#[allow(non_camel_case_types, non_upper_case_globals, dead_code)]
mod cg_ffi {
    use std::ffi::c_void;

    pub type CGEventRef = *mut c_void;
    pub type CGEventSourceRef = *mut c_void;

    #[repr(C)]
    #[derive(Debug, Copy, Clone)]
    pub struct ProcessSerialNumber {
        pub high: u32,
        pub low: u32,
    }

    pub type CGEventFlags = u64;
    pub type CGKeyCode = u16;

    unsafe extern "C" {
        pub fn GetProcessForPID(
            pid: libc::pid_t,
            psn: *mut ProcessSerialNumber,
        ) -> i32; // OSStatus

        pub fn CGEventCreateKeyboardEvent(
            source: CGEventSourceRef,
            virtual_key: CGKeyCode,
            key_down: bool,
        ) -> CGEventRef;

        pub fn CGEventSetFlags(event: CGEventRef, flags: CGEventFlags);

        pub fn CGEventPostToPSN(
            psn: *mut ProcessSerialNumber,
            event: CGEventRef,
        );

        pub fn CFRelease(cf: *mut c_void);
    }
}

/// Map keyboard_types::Code to macOS virtual keycodes.
fn code_to_macos_keycode(code: keyboard_types::Code) -> Option<u16> {
    use keyboard_types::Code::*;
    Some(match code {
        KeyA => 0, KeyS => 1, KeyD => 2, KeyF => 3, KeyH => 4, KeyG => 5,
        KeyZ => 6, KeyX => 7, KeyC => 8, KeyV => 9, KeyB => 11, KeyQ => 12,
        KeyW => 13, KeyE => 14, KeyR => 15, KeyY => 16, KeyT => 17, KeyO => 31,
        KeyU => 32, KeyI => 34, KeyP => 35, KeyL => 37, KeyJ => 38, KeyK => 40,
        KeyN => 45, KeyM => 46,
        Enter => 36, Tab => 48, Space => 49, Backspace => 51, Escape => 53,
        Digit1 => 18, Digit2 => 19, Digit3 => 20, Digit4 => 21, Digit5 => 23,
        Digit6 => 22, Digit7 => 26, Digit8 => 28, Digit9 => 25, Digit0 => 29,
        Minus => 27, Equal => 24, BracketLeft => 33, BracketRight => 30,
        Backslash => 42, Semicolon => 41, Quote => 39, Backquote => 50,
        Comma => 43, Period => 47, Slash => 44,
        F1 => 122, F2 => 120, F3 => 99, F4 => 118, F5 => 96, F6 => 97,
        F7 => 98, F8 => 100, F9 => 101, F10 => 109, F11 => 103, F12 => 111,
        ArrowLeft => 123, ArrowRight => 124, ArrowDown => 125, ArrowUp => 126,
        Home => 115, End => 119, PageUp => 116, PageDown => 121, Delete => 117,
        _ => return None,
    })
}

/// Map keyboard_types::Modifiers to macOS CGEventFlags.
fn modifiers_to_cg_flags(modifiers: keyboard_types::Modifiers) -> u64 {
    let mut flags: u64 = 0;
    if modifiers.contains(keyboard_types::Modifiers::META) {
        flags |= 0x100000; // kCGEventFlagMaskCommand
    }
    if modifiers.contains(keyboard_types::Modifiers::SHIFT) {
        flags |= 0x020000; // kCGEventFlagMaskShift
    }
    if modifiers.contains(keyboard_types::Modifiers::ALT) {
        flags |= 0x080000; // kCGEventFlagMaskAlternate
    }
    if modifiers.contains(keyboard_types::Modifiers::CONTROL) {
        flags |= 0x040000; // kCGEventFlagMaskControl
    }
    flags
}

fn send_keystroke_to_pid(pid: u32, keycode: u16, flags: u64) -> Result<()> {
    use cg_ffi::*;

    unsafe {
        let mut psn = ProcessSerialNumber { high: 0, low: 0 };
        let status = GetProcessForPID(pid as libc::pid_t, &mut psn);
        if status != 0 {
            return Err(Error::Platform(format!(
                "GetProcessForPID failed for pid {pid}: OSStatus {status}"
            )));
        }

        // Key down
        let key_down = CGEventCreateKeyboardEvent(std::ptr::null_mut(), keycode, true);
        if key_down.is_null() {
            return Err(Error::Platform("failed to create key-down event".into()));
        }
        CGEventSetFlags(key_down, flags);
        CGEventPostToPSN(&mut psn, key_down);
        CFRelease(key_down);

        // Key up
        let key_up = CGEventCreateKeyboardEvent(std::ptr::null_mut(), keycode, false);
        if key_up.is_null() {
            return Err(Error::Platform("failed to create key-up event".into()));
        }
        CGEventSetFlags(key_up, flags);
        CGEventPostToPSN(&mut psn, key_up);
        CFRelease(key_up);
    }

    Ok(())
}

/// Check if a process has exited. Returns synthetic ExitStatus.
///
/// macOS GUI apps launched via NSWorkspace are not our child processes,
/// so `waitpid` is not available. We use `kill(pid, 0)` to check liveness.
pub(crate) fn try_wait(pid: u32) -> io::Result<Option<ExitStatus>> {
    let ret = unsafe { libc::kill(pid as i32, 0) };
    if ret == 0 {
        // Process is still running
        Ok(None)
    } else {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            // Process does not exist — it has exited.
            // Synthesize a successful exit status. We can't get the real exit code
            // from a process that isn't our child.
            Ok(Some(make_exit_status(0)))
        } else {
            Err(err)
        }
    }
}

/// Create an ExitStatus from a raw wait status.
/// On Unix, ExitStatus wraps a libc wait status — use `__WEXITSTATUS` encoding.
fn make_exit_status(code: i32) -> ExitStatus {
    // Encode as a normal exit: bits 15:8 = exit code, bits 7:0 = 0 (no signal)
    let wait_status = (code & 0xFF) << 8;
    // SAFETY: ExitStatus on Unix wraps a c_int wait status
    unsafe { std::mem::transmute::<i32, ExitStatus>(wait_status) }
}

/// Single spawn function — handles both piped and non-piped based on Command's stdio config.
pub(crate) fn spawn(cmd: &mut Command) -> Result<crate::Child> {
    let path = &cmd.path;

    if !path.exists() {
        return Err(Error::NotFound(path.display().to_string()));
    }

    let needs_piped = cmd.stdout_cfg.is_some() || cmd.stderr_cfg.is_some();

    if needs_piped {
        spawn_piped(cmd)
    } else {
        spawn_simple(cmd)
    }
}

/// Launch via NSWorkspace (no stdio capture).
fn spawn_simple(cmd: &mut Command) -> Result<crate::Child> {
    let path = &cmd.path;

    // NSWorkspace expects an .app bundle URL. If the user passed the inner
    // binary (e.g. .../Contents/MacOS/RobloxStudio), walk up to find it.
    let bundle_path = find_app_bundle(path)
        .ok_or_else(|| Error::NotFound(format!("no .app bundle found for {}", path.display())))?;

    let path_str = NSString::from_str(&bundle_path.to_string_lossy());
    let url = NSURL::fileURLWithPath(&path_str);

    // Configure launch options
    let config = NSWorkspaceOpenConfiguration::new();
    config.setCreatesNewApplicationInstance(true);
    config.setActivates(!cmd.background);
    if cmd.background {
        config.setHides(true);
    }

    if !cmd.args.is_empty() {
        let ns_args: Vec<Retained<NSString>> =
            cmd.args.iter().map(|a| NSString::from_str(a)).collect();
        let ns_array = NSArray::from_retained_slice(&ns_args);
        config.setArguments(&ns_array);
    }

    // Bridge async completion handler -> synchronous channel
    let (tx, rx) =
        mpsc::channel::<std::result::Result<Retained<NSRunningApplication>, String>>();

    let block = block2::RcBlock::new(
        move |app: *mut NSRunningApplication, error: *mut objc2_foundation::NSError| {
            if app.is_null() {
                let msg = if !error.is_null() {
                    unsafe { &*error }.localizedDescription().to_string()
                } else {
                    "unknown launch error".to_string()
                };
                let _ = tx.send(Err(msg));
            } else {
                let retained = unsafe { Retained::retain(app) }
                    .expect("NSRunningApplication should not be null");
                let _ = tx.send(Ok(retained));
            }
        },
    );

    let workspace = NSWorkspace::sharedWorkspace();

    if let Some(ref open_url) = cmd.url {
        // URL mode: open a URL (e.g. custom://scheme) with the specified app
        let url_str = NSString::from_str(open_url);
        let open_nsurl = NSURL::URLWithString(&url_str)
            .ok_or_else(|| Error::Platform(format!("invalid URL: {open_url}")))?;
        let urls = NSArray::from_retained_slice(&[open_nsurl]);
        workspace.openURLs_withApplicationAtURL_configuration_completionHandler(
            &urls,
            &url,
            &config,
            Some(&block),
        );
    } else {
        // App mode: launch the application directly
        workspace.openApplicationAtURL_configuration_completionHandler(
            &url,
            &config,
            Some(&block),
        );
    }

    // Wait for completion (Studio launch is typically fast, but give it time)
    let result = rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .map_err(|_| Error::Platform("launch timed out after 30s".into()))?
        .map_err(Error::Platform)?;

    let pid = result.processIdentifier();
    if pid < 0 {
        return Err(Error::Platform("process identifier unavailable".into()));
    }

    // Extra hide call in case the app self-activates during startup
    if cmd.background {
        result.hide();
    }

    Ok(crate::Child {
        pid: pid as u32,
        stdout: None,
        stderr: None,
        inner: Some(MacOSHandle { app: result, pid: pid as u32 }),
    })
}

/// Spawn an application with piped stdout/stderr via `/usr/bin/open` + ptys.
///
/// Uses `open -g -j` for clean background launch (no focus steal, no Dock activation).
/// Creates pseudo-terminal pairs for stdout/stderr — unlike FIFOs/pipes, ptys never
/// SIGPIPE or block the writer, so the app can't hang regardless of reader state.
/// Gets `NSRunningApplication` from PID for focus/kill/keystroke support.
fn spawn_piped(cmd: &mut Command) -> Result<crate::Child> {
    let path = &cmd.path;

    if !path.exists() {
        return Err(Error::NotFound(path.display().to_string()));
    }

    // Resolve to .app bundle (open requires bundle path)
    let bundle_path = find_app_bundle(path)
        .ok_or_else(|| Error::NotFound(format!("no .app bundle found for {}", path.display())))?;

    // Snapshot existing PIDs for this bundle to detect the new one after launch
    let bundle_id = bundle_id_from_path(&bundle_path);
    let before_pids: Vec<i32> = if let Some(ref bid) = bundle_id {
        NSRunningApplication::runningApplicationsWithBundleIdentifier(
            &objc2_foundation::NSString::from_str(bid),
        ).iter().map(|app| app.processIdentifier()).collect()
    } else {
        vec![]
    };


    // Create pty pairs for stdout/stderr. Unlike FIFOs/pipes, ptys never
    // send SIGPIPE or block the writer — if nobody reads, data is discarded.
    // This prevents Studio from hanging/crashing regardless of our process state.
    let (stdout_master, stdout_slave_path) = create_pty()?;
    let (stderr_master, stderr_slave_path) = create_pty()?;

    // Build open command: open -n -g -j -a <bundle> --stdout <pty> --stderr <pty> --args <args...>
    let mut open_cmd = std::process::Command::new("/usr/bin/open");
    open_cmd.arg("-n"); // always create new instance
    if cmd.background {
        open_cmd.args(["-g", "-j"]);
    }
    open_cmd.arg("-a").arg(&bundle_path);
    open_cmd.arg("--stdout").arg(&stdout_slave_path);
    open_cmd.arg("--stderr").arg(&stderr_slave_path);
    if !cmd.args.is_empty() {
        open_cmd.arg("--args");
        open_cmd.args(&cmd.args);
    }
    open_cmd.stdout(std::process::Stdio::null());
    open_cmd.stderr(std::process::Stdio::piped());


    let mut open_child = open_cmd.spawn()
        .map_err(|e| Error::Platform(format!("failed to spawn open: {e}")))?;

    // Wait for open to finish and check for errors
    let open_status = open_child.wait()
        .map_err(|e| Error::Platform(format!("failed to wait for open: {e}")))?;
    if !open_status.success() {
        let mut open_stderr = String::new();
        if let Some(mut stderr) = open_child.stderr.take() {
            use std::io::Read;
            let _ = stderr.read_to_string(&mut open_stderr);
        }
        eprintln!("[launch-control] open failed: status={open_status}, stderr={open_stderr}");
    }

    // Find the new PID by diffing against the snapshot
    let pid = if let Some(ref bid) = bundle_id {
        (|| -> Option<u32> {
            for _ in 0..20 {
                let current = NSRunningApplication::runningApplicationsWithBundleIdentifier(
                    &objc2_foundation::NSString::from_str(bid),
                );
                for app in current.iter() {
                    let p = app.processIdentifier();
                    if p > 0 && !before_pids.contains(&p) {
                        return Some(p as u32);
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            None
        })()
        .ok_or_else(|| Error::Platform("could not find launched app PID".into()))?
    } else {
        return Err(Error::Platform("could not determine bundle ID for PID lookup".into()));
    };

    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid as i32);

    // Hide app if background mode — prevents Dock icon from appearing
    if cmd.background {
        if let Some(ref app) = app {
            app.hide();
        }
    }

    let inner = app.map(|app| MacOSHandle { app, pid });

    // Drain threads read from pty master ends.
    // If our process exits, master closes → slave gets EIO (not SIGPIPE) → app handles gracefully.
    let (stdout_tx, stdout_rx) = std::sync::mpsc::channel();
    let (stderr_tx, stderr_rx) = std::sync::mpsc::channel();
    crate::start_drain_thread(stdout_master, stdout_tx);
    crate::start_drain_thread(stderr_master, stderr_tx);

    Ok(crate::Child {
        pid,
        stdout: Some(crate::make_child_stdout(stdout_rx)),
        stderr: Some(crate::make_child_stderr(stderr_rx)),
        inner,
    })
}

/// Create a pty pair and return (master File, slave device path).
/// The slave path (e.g. `/dev/ttys042`) can be passed to `open --stdout/--stderr`.
fn create_pty() -> Result<(std::fs::File, std::path::PathBuf)> {
    use std::os::unix::io::FromRawFd;

    let mut master: libc::c_int = 0;
    let mut slave: libc::c_int = 0;
    let ret = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ret != 0 {
        return Err(Error::Platform(format!(
            "openpty failed: {}",
            std::io::Error::last_os_error()
        )));
    }

    // Get slave device path via ptsname
    let slave_path = unsafe {
        let ptr = libc::ptsname(master);
        if ptr.is_null() {
            libc::close(master);
            libc::close(slave);
            return Err(Error::Platform("ptsname failed".into()));
        }
        std::ffi::CStr::from_ptr(ptr)
            .to_string_lossy()
            .into_owned()
    };

    // Close slave fd — `open` will open the device path itself
    unsafe { libc::close(slave); }

    let master_file = unsafe { std::fs::File::from_raw_fd(master) };
    Ok((master_file, std::path::PathBuf::from(slave_path)))
}

/// Read the bundle identifier from an .app's Info.plist.
fn bundle_id_from_path(bundle_path: &std::path::Path) -> Option<String> {
    let plist_path = bundle_path.join("Contents/Info.plist");
    let content = std::fs::read(&plist_path).ok()?;
    let content_str = String::from_utf8_lossy(&content);
    // Simple XML parse for CFBundleIdentifier
    let key = "<key>CFBundleIdentifier</key>";
    let idx = content_str.find(key)?;
    let after = &content_str[idx + key.len()..];
    let start = after.find("<string>")? + 8;
    let end = after[start..].find("</string>")?;
    Some(after[start..start + end].to_string())
}

/// Walk up from a binary path to find the .app bundle directory.
/// E.g., /Applications/Foo.app/Contents/MacOS/Foo -> /Applications/Foo.app
fn find_app_bundle(path: &std::path::Path) -> Option<std::path::PathBuf> {
    if path.extension().is_some_and(|e| e == "app") {
        return Some(path.to_path_buf());
    }
    let mut current = path.to_path_buf();
    while let Some(parent) = current.parent() {
        if parent.extension().is_some_and(|e| e == "app") {
            return Some(parent.to_path_buf());
        }
        if parent == current {
            break;
        }
        current = parent.to_path_buf();
    }
    None
}
