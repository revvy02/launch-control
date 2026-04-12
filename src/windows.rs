use crate::error::{Error, Result};
use crate::AppLauncher;

use std::mem;
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, BOOL, HANDLE, HWND, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, TerminateProcess, WaitForSingleObject, PROCESS_INFORMATION, STARTF_USESHOWWINDOW,
    STARTUPINFOW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, SW_SHOWNORMAL, SW_SHOWMINNOACTIVE,
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
    pub fn is_running(&self) -> bool {
        unsafe { WaitForSingleObject(self.process_handle, 0) == WAIT_TIMEOUT }
    }

    pub fn focus(&self) -> Result<()> {
        if !self.is_running() {
            return Err(Error::Terminated);
        }
        let hwnd = find_window_by_pid(self.pid)
            .ok_or_else(|| Error::Platform("no window found for process".into()))?;

        unsafe {
            ShowWindow(hwnd, SW_SHOWNORMAL);
            SetForegroundWindow(hwnd);
        }
        Ok(())
    }

    pub fn kill(&self) -> Result<()> {
        if !self.is_running() {
            return Ok(());
        }
        let ok = unsafe { TerminateProcess(self.process_handle, 1) };
        if ok == 0 {
            Err(Error::Platform("TerminateProcess failed".into()))
        } else {
            Ok(())
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

pub(crate) fn spawn(launcher: AppLauncher) -> Result<crate::AppHandle> {
    let exe_path = &launcher.path;
    if !exe_path.exists() {
        return Err(Error::NotFound(exe_path.display().to_string()));
    }

    // Build command line: "exe_path" arg1 arg2 ...
    let mut cmd_line = format!("\"{}\"", exe_path.display());
    for arg in &launcher.args {
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
    si.wShowWindow = if launcher.background {
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

    Ok(crate::AppHandle {
        pid,
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
