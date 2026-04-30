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
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

/// Serializes the snapshot → open → PID-claim window in `spawn_piped`.
/// The lookup uses NSRunningApplication's bundle-ID set diff, which is
/// inherently racy under concurrent spawns of the same bundle: multiple
/// callers see the same "first new PID" and the rest are orphaned (never
/// associated with any callback / Child handle). Holding this lock across
/// the snapshot/launch/claim ensures each caller is paired with a unique
/// new PID.
static SPAWN_PIPED_LOCK: Mutex<()> = Mutex::new(());

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

// CoreGraphics FFI for posting keyboard events.
#[allow(non_camel_case_types, non_upper_case_globals, dead_code)]
mod cg_ffi {
    use std::ffi::c_void;

    pub type CGEventRef = *mut c_void;
    pub type CGEventSourceRef = *mut c_void;
    pub type CGEventFlags = u64;
    pub type CGKeyCode = u16;

    // CGEventTapLocation values — where the event enters the event stream.
    // kCGSessionEventTap (1) is the window-server session queue; NSApp of the
    // frontmost app drains it continuously, which is why synthetic Cmd+S via
    // this path triggers Qt shortcuts even when the app was already Active.
    pub const K_CG_HID_EVENT_TAP: u32 = 0;
    pub const K_CG_SESSION_EVENT_TAP: u32 = 1;
    #[allow(dead_code)]
    pub const K_CG_ANNOTATED_SESSION_EVENT_TAP: u32 = 2;

    unsafe extern "C" {
        pub fn CGEventCreateKeyboardEvent(
            source: CGEventSourceRef,
            virtual_key: CGKeyCode,
            key_down: bool,
        ) -> CGEventRef;

        pub fn CGEventSetFlags(event: CGEventRef, flags: CGEventFlags);

        // Post to the session event tap — same path hardware events take.
        // Routes to whichever app is currently frontmost. Use this when the
        // target is already frontmost so NSApp's normal event pump delivers
        // the event (vs CGEventPostToPid which writes to the target's private
        // Carbon queue, drained by Qt only on inactive→active transitions).
        pub fn CGEventPost(tap: u32, event: CGEventRef);

        // Post a CGEvent to a specific process by PID. macOS 10.11+.
        // Preferred over CGEventPostToPSN which requires a Carbon PSN lookup
        // via GetProcessForPID — both deprecated, and the PSN path fails for
        // background-launched processes that haven't registered with Launch
        // Services yet. The trade-off: events land in the target's private
        // Carbon queue, which Qt only drains on activation-state transitions,
        // so this is unreliable when the target is already frontmost.
        pub fn CGEventPostToPid(pid: libc::pid_t, event: CGEventRef);

        pub fn CFRelease(cf: *mut c_void);
    }
}

/// Returns true if the given PID is currently the frontmost application
/// according to NSWorkspace. Used to pick between `CGEventPost` (for
/// frontmost targets) and `CGEventPostToPid` (for everything else).
fn is_pid_frontmost(pid: u32) -> bool {
    let workspace = NSWorkspace::sharedWorkspace();
    match workspace.frontmostApplication() {
        Some(app) => app.processIdentifier() == pid as libc::pid_t,
        None => false,
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

    // Route by frontmost-ness:
    //
    // - `CGEventPostToPid` writes to the target process's private Carbon
    //   event queue. A Qt app only drains that queue on an inactive→active
    //   transition, so posting here when the target is already frontmost
    //   results in silently dropped events (observed: foreground Studio never
    //   receiving Cmd+S).
    // - `CGEventPost(kCGSessionEventTap)` posts to the window server's
    //   session queue — the same queue hardware events take. NSApp of the
    //   frontmost app drains it continuously, so a synthetic Cmd+S there
    //   triggers shortcuts exactly like a real key press.
    //
    // Picked PostToPid for backgrounded targets because session-tap events
    // only go to whichever app is frontmost; we'd otherwise send Cmd+S to
    // the wrong app. Apple's own guidance (NSEvent docs): "CGEventPostToPSN
    // if you want to target a specific process, or post to kCGSessionEventTap
    // if you want to target the app with focus."
    let frontmost = is_pid_frontmost(pid);
    let actual_frontmost_pid = {
        let workspace = NSWorkspace::sharedWorkspace();
        workspace.frontmostApplication().map(|a| a.processIdentifier() as u32)
    };
    tracing::info!(
        pid,
        frontmost,
        actual_frontmost_pid = ?actual_frontmost_pid,
        route = if frontmost { "session_tap" } else { "post_to_pid" },
        "send_keystroke routing"
    );

    unsafe {
        let key_down = CGEventCreateKeyboardEvent(std::ptr::null_mut(), keycode, true);
        if key_down.is_null() {
            return Err(Error::Platform("failed to create key-down event".into()));
        }
        CGEventSetFlags(key_down, flags);
        if frontmost {
            CGEventPost(K_CG_SESSION_EVENT_TAP, key_down);
        } else {
            CGEventPostToPid(pid as libc::pid_t, key_down);
        }
        CFRelease(key_down);

        let key_up = CGEventCreateKeyboardEvent(std::ptr::null_mut(), keycode, false);
        if key_up.is_null() {
            return Err(Error::Platform("failed to create key-up event".into()));
        }
        CGEventSetFlags(key_up, flags);
        if frontmost {
            CGEventPost(K_CG_SESSION_EVENT_TAP, key_up);
        } else {
            CGEventPostToPid(pid as libc::pid_t, key_up);
        }
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

    let exit_state = std::sync::Arc::new(std::sync::Mutex::new(crate::ExitState::default()));
    crate::start_exit_watcher(pid as u32, exit_state.clone());

    Ok(crate::Child {
        pid: pid as u32,
        stdout: None,
        stderr: None,
        exit_state,
        inner: Some(MacOSHandle { app: result, pid: pid as u32 }),
    })
}

/// Spawn an application with piped stdout/stderr via the helper subprocess.
///
/// The helper (`launch-control`, this crate's binary target) holds the pty
/// masters in its own process. This makes the launched app's stdio survive
/// the calling process's death — for `Command::detached(true)`, when the
/// caller dies, the helper survives and the app keeps running with stdio
/// captured. For non-detached, the helper exits when its parent dies, the
/// pty masters close, and the app receives SIGHUP and dies (orderly cascade).
///
/// Uses `open -g -j` (inside the helper) for clean background launch (no
/// focus steal, no Dock activation). Returns an `NSRunningApplication`-backed
/// `Child` for focus/kill/keystroke support.
fn spawn_piped(cmd: &mut Command) -> Result<crate::Child> {
    let path = &cmd.path;

    if !path.exists() {
        return Err(Error::NotFound(path.display().to_string()));
    }

    // Resolve to .app bundle (the helper's `open` invocation requires it)
    let bundle_path = find_app_bundle(path)
        .ok_or_else(|| Error::NotFound(format!("no .app bundle found for {}", path.display())))?;
    let bundle_id = bundle_id_from_path(&bundle_path)
        .ok_or_else(|| Error::Platform("could not determine bundle ID for PID lookup".into()))?;

    // Find the helper binary: sibling to current_exe by default, or
    // overridden via env var (useful when consumer's build deploys it
    // alongside but renamed, or for tests).
    let helper_bin = resolve_binary()?;

    // Hold the spawn lock across the snapshot → spawn-helper → PID-claim
    // window. NSRunningApplication's set-diff is racy under concurrent spawns
    // of the same bundle.
    let _spawn_guard = SPAWN_PIPED_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // Snapshot existing PIDs for this bundle to detect the new one after launch
    let before_pids: Vec<i32> = NSRunningApplication::runningApplicationsWithBundleIdentifier(
        &objc2_foundation::NSString::from_str(&bundle_id),
    ).iter().map(|app| app.processIdentifier()).collect();

    // Spawn the helper. The helper does the actual NSWorkspace+pty work
    // internally and forwards the app's stdio to its own stdout/stderr.
    let mut helper_cmd = std::process::Command::new(&helper_bin);
    helper_cmd.arg("--bundle").arg(&bundle_path);
    if cmd.background {
        helper_cmd.arg("--background");
    }
    if cmd.detached {
        helper_cmd.arg("--detached");
    }
    if !cmd.args.is_empty() {
        helper_cmd.arg("--");
        helper_cmd.args(&cmd.args);
    }
    helper_cmd.stdin(std::process::Stdio::null());
    helper_cmd.stdout(std::process::Stdio::piped());
    helper_cmd.stderr(std::process::Stdio::piped());

    let mut helper_child = helper_cmd.spawn()
        .map_err(|e| Error::Platform(format!("failed to spawn launch-control helper ({}): {e}", helper_bin.display())))?;

    // Find the new PID by diffing against the snapshot. The helper kicks off
    // `open` which spawns the app; the app is in NSRunningApplication's list
    // shortly after.
    let pid = (|| -> Option<u32> {
        for _ in 0..50 {
            let current = NSRunningApplication::runningApplicationsWithBundleIdentifier(
                &objc2_foundation::NSString::from_str(&bundle_id),
            );
            for app in current.iter() {
                let p = app.processIdentifier();
                if p > 0 && !before_pids.contains(&p) {
                    return Some(p as u32);
                }
            }
            // Helper bails fast on bad args; if it died before the app
            // appears, give up rather than wait the full 10s.
            if let Ok(Some(_status)) = helper_child.try_wait() {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        None
    })()
    .ok_or_else(|| {
        // Drain helper stderr for diagnostics
        let mut helper_stderr = String::new();
        if let Some(mut stderr) = helper_child.stderr.take() {
            use std::io::Read;
            let _ = stderr.read_to_string(&mut helper_stderr);
        }
        Error::Platform(format!("helper failed to launch app: {helper_stderr}"))
    })?;

    // PID claimed — release the spawn lock.
    drop(_spawn_guard);

    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid as i32);

    // Hide app if background mode — prevents Dock icon from appearing
    if cmd.background {
        if let Some(ref app) = app {
            app.hide();
        }
    }

    let inner = app.map(|app| MacOSHandle { app, pid });

    // Drain helper subprocess stdout/stderr (each carries the app's
    // corresponding stream, forwarded by the helper's pty drain). These are
    // real OS pipes between us and the helper — no pty involved at this
    // layer, so we don't get SIGHUP'd if the helper dies later.
    let (stdout_tx, stdout_rx) = std::sync::mpsc::channel();
    let (stderr_tx, stderr_rx) = std::sync::mpsc::channel();
    if let Some(stdout) = helper_child.stdout.take() {
        crate::start_drain_thread(stdout, stdout_tx);
    }
    if let Some(stderr) = helper_child.stderr.take() {
        crate::start_drain_thread(stderr, stderr_tx);
    }

    let exit_state = std::sync::Arc::new(std::sync::Mutex::new(crate::ExitState::default()));
    crate::start_exit_watcher(pid, exit_state.clone());

    // Forget the helper handle — we don't want std::process::Child::Drop
    // killing the helper when this scope ends. Helper lifecycle is tied to
    // the app's via parent-exit + pty SIGHUP; explicit Child::kill() on our
    // returned handle goes via the app's NSRunningApplication, and the
    // helper sees the app exit and tears itself down.
    std::mem::forget(helper_child);

    Ok(crate::Child {
        pid,
        stdout: Some(crate::make_child_stdout(stdout_rx)),
        stderr: Some(crate::make_child_stderr(stderr_rx)),
        exit_state,
        inner,
    })
}

/// Locate the `launch-control` helper binary. Override with the
/// `LAUNCH_CONTROL_BIN` env var; otherwise look for it as a sibling of the
/// current executable (typical layout: `bin/<consumer>` and
/// `bin/launch-control` in the same directory).
fn resolve_binary() -> Result<std::path::PathBuf> {
    if let Ok(p) = std::env::var("LAUNCH_CONTROL_BIN") {
        let path = std::path::PathBuf::from(p);
        if path.exists() {
            return Ok(path);
        }
        return Err(Error::Platform(format!(
            "LAUNCH_CONTROL_BIN points to non-existent path: {}",
            path.display()
        )));
    }
    let exe = std::env::current_exe()
        .map_err(|e| Error::Platform(format!("current_exe failed: {e}")))?;
    let dir = exe.parent()
        .ok_or_else(|| Error::Platform("current_exe has no parent dir".into()))?;
    let candidate = dir.join("launch-control");
    if candidate.exists() {
        return Ok(candidate);
    }
    Err(Error::Platform(format!(
        "launch-control helper not found at {} (set LAUNCH_CONTROL_BIN to override)",
        candidate.display()
    )))
}

/// Body of the `launch-control` helper binary. Lives here so it's testable
/// lib code; the binary target is a 3-line shim that calls this.
///
/// Reads args:
///   `--bundle <path>` (required) — `.app` bundle path
///   `--background`               — pass `-g -j` to `open` (no focus steal)
///   `--detached`                 — survive parent death (don't exit on
///                                   parent-pid disappearance)
///   `--`  ... app args ...
///
/// Behavior:
///   1. Ignore SIGPIPE so writes to a closed parent stdout/stderr don't kill
///      us in detached mode.
///   2. Create stdout/stderr ptys, spawn `/usr/bin/open` with `--stdout`/
///      `--stderr` set to the slave paths.
///   3. Drain pty masters → forward each line to helper's own stdout/stderr.
///      When the app exits, its pty slave closes → master gets EIO → drain
///      thread ends.
///   4. If `!--detached`, watch the parent PID via `parent-exit`; on parent
///      death, exit (which closes the pty masters → app gets SIGHUP → orderly
///      cascade kill).
///   5. Block on the drain threads. When both finish (app exited), exit 0.
pub(crate) fn run_helper_main() -> ! {
    // 1. Become our own session leader (`setsid`) so the helper is detached
    //    from the parent's session and process group. Without this, when the
    //    session leader (typically the shell / test runner that spawned the
    //    consumer process) dies, the kernel delivers SIGHUP to every process
    //    in the session — helper included — which kills detached mode.
    //
    //    Also explicitly ignore SIGPIPE (writes to a closed parent stdio
    //    pipe shouldn't kill us — we drain to void instead) and SIGHUP
    //    (defense in depth in case a controlling terminal we don't know
    //    about hangs up).
    unsafe {
        libc::setsid();
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }

    // Parse args
    let mut bundle: Option<std::path::PathBuf> = None;
    let mut background = false;
    let mut detached = false;
    let mut app_args: Vec<String> = Vec::new();
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--bundle" => {
                bundle = iter.next().map(std::path::PathBuf::from);
            }
            "--background" => background = true,
            "--detached" => detached = true,
            "--" => {
                app_args.extend(iter.by_ref());
                break;
            }
            other => {
                eprintln!("launch-control: unknown arg: {other}");
                std::process::exit(2);
            }
        }
    }

    let bundle = match bundle {
        Some(b) => b,
        None => {
            eprintln!("launch-control: --bundle <path> required");
            std::process::exit(2);
        }
    };

    let log_path = std::env::temp_dir().join(format!("launch-control-{}.log", std::process::id()));
    fn log_to(path: &std::path::Path, msg: &str) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            let _ = writeln!(f, "[{:.3}] [{}] {}", now, std::process::id(), msg);
        }
    }
    log_to(&log_path, &format!(
        "starting pid={} ppid={} detached={} background={} bundle={}",
        std::process::id(),
        unsafe { libc::getppid() },
        detached,
        background,
        bundle.display(),
    ));

    // 2. Create ptys + spawn `open`
    let (stdout_master, stdout_slave_path) = match create_pty() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("launch-control: create_pty(stdout) failed: {e}");
            std::process::exit(1);
        }
    };
    let (stderr_master, stderr_slave_path) = match create_pty() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("launch-control: create_pty(stderr) failed: {e}");
            std::process::exit(1);
        }
    };

    let mut open_cmd = std::process::Command::new("/usr/bin/open");
    open_cmd.arg("-n");
    if background {
        open_cmd.args(["-g", "-j"]);
    }
    open_cmd.arg("-a").arg(&bundle);
    open_cmd.arg("--stdout").arg(&stdout_slave_path);
    open_cmd.arg("--stderr").arg(&stderr_slave_path);
    if !app_args.is_empty() {
        open_cmd.arg("--args");
        open_cmd.args(&app_args);
    }
    open_cmd.stdout(std::process::Stdio::null());
    open_cmd.stderr(std::process::Stdio::piped());

    let mut open_child = match open_cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("launch-control: failed to spawn /usr/bin/open: {e}");
            std::process::exit(1);
        }
    };
    let open_status = open_child.wait().ok();
    if !open_status.map(|s| s.success()).unwrap_or(false) {
        let mut open_stderr = String::new();
        if let Some(mut stderr) = open_child.stderr.take() {
            use std::io::Read;
            let _ = stderr.read_to_string(&mut open_stderr);
        }
        eprintln!("launch-control: open failed: {open_stderr}");
        std::process::exit(1);
    }

    log_to(&log_path, "open completed; about to find Studio PID via NSRunningApplication");

    // Find the Studio PID (just for logging; not strictly needed by helper)
    let bundle_id = bundle_id_from_path(&bundle).unwrap_or_default();
    let studio_pid: Option<i32> = if !bundle_id.is_empty() {
        let apps = NSRunningApplication::runningApplicationsWithBundleIdentifier(
            &objc2_foundation::NSString::from_str(&bundle_id),
        );
        let mut newest: Option<(std::time::SystemTime, i32)> = None;
        for app in apps.iter() {
            let p = app.processIdentifier();
            if p > 0 {
                newest = match newest {
                    None => Some((std::time::SystemTime::now(), p)),
                    Some((t, _)) => Some((t, p)),
                };
            }
        }
        newest.map(|(_, p)| p)
    } else {
        None
    };
    log_to(&log_path, &format!("found studio_pid={studio_pid:?}; starting drain threads"));

    // Watch Studio's exit so we log when it dies and can correlate timing
    if let Some(pid) = studio_pid {
        let lp = log_path.clone();
        parent_exit::on_pid_exit(pid as u32, move || {
            log_to(&lp, &format!("studio_pid={pid} EXITED"));
        });
    }

    // 3. Drain pty masters → helper's stdout/stderr (real pipes to parent)
    let stdout_handle = std::thread::spawn(move || {
        relay_pty_to(stdout_master, std::io::stdout());
    });
    let stderr_handle = std::thread::spawn(move || {
        relay_pty_to(stderr_master, std::io::stderr());
    });

    // 4. Watch parent PID. On Unix `getppid()` returns the immediate parent;
    // for our use case (spawned by launch-control's `Command::spawn`), that's
    // the consumer process (e.g. rodeo serve). If non-detached and parent
    // dies, exit — that closes our pty masters, app gets SIGHUP, dies.
    if !detached {
        let parent_pid = unsafe { libc::getppid() } as u32;
        log_to(&log_path, &format!("non-detached: registering parent_exit watch on pid={parent_pid}"));
        if parent_pid > 1 {
            let log_path_for_handler = log_path.clone();
            parent_exit::on_pid_exit(parent_pid, move || {
                log_to(&log_path_for_handler, "parent_exit handler fired; calling exit(0)");
                std::process::exit(0);
            });
        }
    } else {
        log_to(&log_path, "detached: skipping parent_exit registration");
    }

    // 5. Block on drains. When both end (pty EOF — app exited), helper exits.
    log_to(&log_path, "blocking on drain threads");
    let _ = stdout_handle.join();
    log_to(&log_path, "stdout drain finished");
    let _ = stderr_handle.join();
    log_to(&log_path, "stderr drain finished; exiting 0");
    std::process::exit(0);
}

/// Drain a pty master into a writer (helper's stdout/stderr) line-buffered.
/// If the writer fails (parent stdio closed), keep draining the pty into the
/// void so the app doesn't fill its pty buffer and stall.
fn relay_pty_to<W: std::io::Write>(reader: std::fs::File, mut writer: W) {
    use std::io::{BufRead, BufReader, Read};
    let log_path = std::env::temp_dir().join(format!("launch-control-{}.log", std::process::id()));
    let log = |msg: &str| {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
            let _ = writeln!(f, "[{}] relay_pty: {msg}", std::process::id());
        }
    };
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    let mut sink = false;
    let mut byte_buf = [0u8; 4096];
    loop {
        if sink {
            // Parent closed; keep pty draining via raw reads.
            line.clear();
            match buf_reader.read(&mut byte_buf) {
                Ok(0) => { log("EOF (sink mode)"); return; }
                Err(e) => { log(&format!("read err in sink: {e}")); return; }
                Ok(_) => continue,
            }
        }
        line.clear();
        match buf_reader.read_line(&mut line) {
            Ok(0) => { log("EOF (line mode)"); return; }
            Ok(_) => {
                if writer.write_all(line.as_bytes()).is_err() {
                    let _ = writer.flush();
                    sink = true;
                    log("write err → entering sink mode");
                    continue;
                }
                let _ = writer.flush();
            }
            Err(e) => { log(&format!("read_line err: {e}")); return; }
        }
    }
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
