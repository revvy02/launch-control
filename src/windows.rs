use crate::error::{Error, Result};
use crate::Command;

use std::io;
use std::mem;
use std::process::ExitStatus;
use std::ptr;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, BOOL, HANDLE, HWND, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, TerminateProcess, WaitForSingleObject,
    PROCESS_INFORMATION, STARTF_USESHOWWINDOW, STARTUPINFOW, INFINITE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindowThreadProcessId, SW_SHOWNORMAL, SW_SHOWMINNOACTIVE,
    SetForegroundWindow, ShowWindow,
};

/// Windows-specific handle wrapping a process HANDLE.
pub(crate) struct WindowsHandle {
    process_handle: HANDLE,
    pid: u32,
}

// HANDLE is Send+Sync safe on Windows (opaque kernel object reference).
unsafe impl Send for WindowsHandle {}
unsafe impl Sync for WindowsHandle {}

impl WindowsHandle {
    pub fn focus(&self) -> Result<()> {
        let hwnd = find_window_by_pid(self.pid)
            .ok_or_else(|| Error::Platform("no window found for process".into()))?;

        // Re-request foreground every tick. SetForegroundWindow can silently
        // fail under Windows's focus-stealing-prevention heuristics or when
        // other processes are competing for focus. Repeat until
        // GetForegroundWindow confirms our pid owns it.
        //
        // Menu shortcuts (Ctrl+S, etc.) need our window to be foreground;
        // callers that follow focus() with send_keystroke depend on this.
        let deadline = Instant::now() + Duration::from_secs(5);
        let started = Instant::now();
        let mut ticks = 0u32;
        loop {
            unsafe {
                ShowWindow(hwnd, SW_SHOWNORMAL);
                SetForegroundWindow(hwnd);
            }
            let front_pid: u32 = unsafe {
                let front_hwnd = GetForegroundWindow();
                if !front_hwnd.is_null() {
                    let mut pid: u32 = 0;
                    GetWindowThreadProcessId(front_hwnd, &mut pid);
                    pid
                } else {
                    0
                }
            };
            if front_pid == self.pid {
                tracing::debug!(
                    pid = self.pid,
                    ticks,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "focus: confirmed foreground",
                );
                return Ok(());
            }
            ticks += 1;
            if ticks % 5 == 0 {
                tracing::debug!(
                    target_pid = self.pid,
                    front_pid,
                    ticks,
                    "focus: still waiting, another process is foreground",
                );
            }
            if Instant::now() >= deadline {
                tracing::warn!(
                    target_pid = self.pid,
                    front_pid,
                    ticks,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "focus: gave up after 5s, another process holds foreground",
                );
                return Err(Error::Platform(format!(
                    "focus: pid {} not foreground after 5s (foreground was pid {})",
                    self.pid, front_pid
                )));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Kill returning io::Error (for Child::kill compatibility).
    pub fn kill_io(&self) -> io::Result<()> {
        // Check if still running first
        let wait_result = unsafe { WaitForSingleObject(self.process_handle, 0) };
        if wait_result != WAIT_TIMEOUT {
            return Ok(()); // already dead
        }
        let ok = unsafe { TerminateProcess(self.process_handle, 1) };
        if ok == 0 {
            Err(io::Error::new(io::ErrorKind::Other, "TerminateProcess failed"))
        } else {
            Ok(())
        }
    }

    /// Check if the process has exited. Returns ExitStatus if exited.
    pub fn try_wait(&self) -> io::Result<Option<ExitStatus>> {
        let wait_result = unsafe { WaitForSingleObject(self.process_handle, 0) };
        if wait_result == WAIT_TIMEOUT {
            return Ok(None); // still running
        }
        if wait_result == WAIT_OBJECT_0 {
            let mut exit_code: u32 = 0;
            let ok = unsafe { GetExitCodeProcess(self.process_handle, &mut exit_code) };
            if ok == 0 {
                return Err(io::Error::new(io::ErrorKind::Other, "GetExitCodeProcess failed"));
            }
            // On Windows, ExitStatus wraps a u32 exit code
            Ok(Some(unsafe { std::mem::transmute::<u32, ExitStatus>(exit_code) }))
        } else {
            Err(io::Error::new(io::ErrorKind::Other, "WaitForSingleObject failed"))
        }
    }
}

impl Drop for WindowsHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.process_handle);
        }
    }
}

pub(crate) fn spawn(cmd: &mut Command) -> Result<crate::Child> {
    let exe_path = &cmd.path;
    if !exe_path.exists() {
        return Err(Error::NotFound(exe_path.display().to_string()));
    }

    // Build command line: "exe_path" arg1 arg2 ...
    let mut cmd_line = format!("\"{}\"", exe_path.display());
    for arg in &cmd.args {
        cmd_line.push(' ');
        if arg.contains(' ') || arg.contains('"') {
            cmd_line.push('"');
            cmd_line.push_str(&arg.replace('"', "\\\""));
            cmd_line.push('"');
        } else {
            cmd_line.push_str(arg);
        }
    }

    // Convert to null-terminated UTF-16
    let mut wide_cmd: Vec<u16> = cmd_line.encode_utf16().collect();
    wide_cmd.push(0);

    let mut si: STARTUPINFOW = unsafe { mem::zeroed() };
    si.cb = mem::size_of::<STARTUPINFOW>() as u32;
    si.dwFlags = STARTF_USESHOWWINDOW;
    si.wShowWindow = if cmd.background {
        SW_SHOWMINNOACTIVE as u16
    } else {
        SW_SHOWNORMAL as u16
    };

    let mut pi: PROCESS_INFORMATION = unsafe { mem::zeroed() };

    let ok = unsafe {
        CreateProcessW(
            ptr::null(),
            wide_cmd.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0, // bInheritHandles = FALSE
            0, // dwCreationFlags
            ptr::null(),
            ptr::null(),
            &si,
            &mut pi,
        )
    };

    if ok == 0 {
        return Err(Error::Platform("CreateProcessW failed".into()));
    }

    // Close thread handle immediately (only need the process handle)
    unsafe {
        CloseHandle(pi.hThread);
    }

    let pid = pi.dwProcessId;

    let exit_state = std::sync::Arc::new(std::sync::Mutex::new(crate::ExitState::default()));
    crate::start_exit_watcher(pid, exit_state.clone());

    Ok(crate::Child {
        pid,
        stdout: None,
        stderr: None,
        exit_state,
        inner: WindowsHandle {
            process_handle: pi.hProcess,
            pid,
        },
    })
}


/// Find the first top-level window belonging to the given PID.
fn find_window_by_pid(target_pid: u32) -> Option<HWND> {
    struct FindData {
        target_pid: u32,
        found_hwnd: Option<HWND>,
    }

    unsafe extern "system" fn enum_callback(hwnd: HWND, lparam: isize) -> BOOL {
        let data = &mut *(lparam as *mut FindData);
        let mut window_pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut window_pid);
        if window_pid == data.target_pid {
            data.found_hwnd = Some(hwnd);
            return 0; // stop enumeration
        }
        1 // continue
    }

    let mut data = FindData {
        target_pid,
        found_hwnd: None,
    };

    unsafe {
        EnumWindows(Some(enum_callback), &mut data as *mut _ as isize);
    }

    data.found_hwnd
}
