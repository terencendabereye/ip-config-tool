//! Windows ConPTY (`CreatePseudoConsole`) wrapper: spawns a child console
//! process attached to a real pseudo-console instead of a plain pipe, so
//! interactive programs (SSH in particular) get proper password masking,
//! arrow-key history, and ANSI color/cursor support. See `gui::App`'s
//! `pty_sessions` for how output is drained on the UI-thread timer tick,
//! matching the existing scan/autodetect/identify background-thread pattern.

use std::io::Read;
use std::ptr;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use winapi::shared::minwindef::DWORD;
use winapi::shared::ntdef::HANDLE;
use winapi::shared::winerror::S_OK;
use winapi::um::consoleapi::{ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole};
use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
use winapi::um::namedpipeapi::CreatePipe;
use winapi::um::processthreadsapi::{
    CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList, PROCESS_INFORMATION,
    TerminateProcess, UpdateProcThreadAttribute,
};
use winapi::um::synchapi::WaitForSingleObject;
use winapi::um::winbase::{EXTENDED_STARTUPINFO_PRESENT, STARTUPINFOEXW};
use winapi::um::wincontypes::{COORD, HPCON};
use winapi::um::winnt::HRESULT;

/// Undocumented-by-winapi constant for `UpdateProcThreadAttribute`'s
/// attribute parameter; not exposed by the `winapi` crate itself.
const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;
const WAIT_TIMEOUT: DWORD = 258;

/// Thin RAII wrapper around a raw Win32 `HANDLE`.
struct OwnedHandle(HANDLE);

unsafe impl Send for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// A live ConPTY-backed child process (`ping`, `tracert`, `ssh`, ...).
///
/// Reading happens on a dedicated background thread that pushes raw output
/// chunks through `output_rx`; the UI thread drains that channel on its
/// existing timer tick rather than blocking on I/O itself.
pub struct PtySession {
    hpcon: HPCON,
    input_write: OwnedHandle,
    process: OwnedHandle,
    pub output_rx: Receiver<Vec<u8>>,
}

unsafe impl Send for PtySession {}

impl PtySession {
    /// Spawns `program args...` attached to a new pseudo-console of size
    /// `cols x rows`. Returns `Err` if ConPTY is unavailable (pre-1809
    /// Windows) or process creation otherwise fails — callers should fall
    /// back to `tools::launch_visible` in that case.
    pub fn spawn(program: &str, args: &[&str], cols: u16, rows: u16) -> Result<PtySession, String> {
        unsafe { spawn_impl(program, args, cols, rows) }
    }

    pub fn write_input(&self, bytes: &[u8]) {
        use std::os::windows::io::FromRawHandle;
        // `WriteFile` isn't in the feature set we enabled; std's File over
        // the raw handle gives us a safe, already-tested write path. The
        // handle is duplicated as a `ManuallyDrop`-free borrow by not
        // letting the File close it: we `into_raw_handle` it right back out.
        let mut file = unsafe { std::fs::File::from_raw_handle(self.input_write.0 as *mut _) };
        use std::io::Write;
        let _ = file.write_all(bytes);
        let _ = file.flush();
        std::mem::forget(file); // we still own input_write; don't let File close it
    }

    /// Not currently called — `gui.rs` uses a fixed terminal grid size (see
    /// its `TERMINAL_COLS`/`TERMINAL_ROWS` comment for why), but this stays
    /// available for whoever wires up live resize-on-drag later.
    #[allow(dead_code)]
    pub fn resize(&self, cols: u16, rows: u16) {
        let size = COORD { X: cols as i16, Y: rows as i16 };
        unsafe { ResizePseudoConsole(self.hpcon, size) };
    }

    /// Non-blocking check for whether the child has exited.
    pub fn has_exited(&self) -> bool {
        unsafe { WaitForSingleObject(self.process.0, 0) != WAIT_TIMEOUT }
    }

    /// Forcibly ends the child process. Safe to call more than once, and
    /// safe to call on an already-exited process. Used when a tab or the
    /// whole app is closed with a session (e.g. `ping -t`) still running —
    /// closing the pty's pipes alone doesn't reliably stop every program.
    pub fn kill(&self) {
        unsafe { TerminateProcess(self.process.0, 0) };
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        self.kill();
        unsafe { ClosePseudoConsole(self.hpcon) };
        // input_write / process handles close themselves via OwnedHandle's Drop.
    }
}

unsafe fn spawn_impl(program: &str, args: &[&str], cols: u16, rows: u16) -> Result<PtySession, String> {
    // Pipe pair for the pty's input: we write to `input_write`, ConPTY
    // reads from `input_read` (which we hand off and then close our copy of).
    let (input_read, input_write) = create_pipe()?;
    // Pipe pair for the pty's output: ConPTY writes to `output_write`, we
    // read from `output_read` on the background thread.
    let (output_read, output_write) = create_pipe()?;

    let size = COORD { X: cols as i16, Y: rows as i16 };
    let mut hpcon: HPCON = ptr::null_mut();
    let hr: HRESULT = CreatePseudoConsole(size, input_read.0, output_write.0, 0, &mut hpcon);
    // ConPTY duplicates the handles it needs internally; the ends we handed
    // in are no longer needed on our side once creation returns.
    drop(input_read);
    drop(output_write);
    if hr != S_OK {
        return Err(format!("CreatePseudoConsole failed (hresult 0x{hr:08X})"));
    }

    let spawn_result = spawn_attached(program, args, hpcon);
    let process = match spawn_result {
        Ok(p) => p,
        Err(e) => {
            ClosePseudoConsole(hpcon);
            return Err(e);
        }
    };

    let (tx, rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = mpsc::channel();
    spawn_reader_thread(output_read, tx);

    Ok(PtySession { hpcon, input_write, process, output_rx: rx })
}

unsafe fn create_pipe() -> Result<(OwnedHandle, OwnedHandle), String> {
    let mut read_handle: HANDLE = ptr::null_mut();
    let mut write_handle: HANDLE = ptr::null_mut();
    let ok = CreatePipe(&mut read_handle, &mut write_handle, ptr::null_mut(), 0);
    if ok == 0 {
        return Err("failed to create pipe".to_string());
    }
    Ok((OwnedHandle(read_handle), OwnedHandle(write_handle)))
}

unsafe fn spawn_attached(program: &str, args: &[&str], hpcon: HPCON) -> Result<OwnedHandle, String> {
    let mut attr_list_size: usize = 0;
    InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut attr_list_size);

    let mut attr_list_buf = vec![0u8; attr_list_size];
    let attr_list = attr_list_buf.as_mut_ptr() as winapi::um::processthreadsapi::LPPROC_THREAD_ATTRIBUTE_LIST;
    if InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_list_size) == 0 {
        return Err("InitializeProcThreadAttributeList failed".to_string());
    }

    let update_ok = UpdateProcThreadAttribute(
        attr_list,
        0,
        PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
        hpcon as *mut _,
        std::mem::size_of::<HPCON>(),
        ptr::null_mut(),
        ptr::null_mut(),
    );
    if update_ok == 0 {
        DeleteProcThreadAttributeList(attr_list);
        return Err("UpdateProcThreadAttribute failed".to_string());
    }

    let mut startup_info: STARTUPINFOEXW = std::mem::zeroed();
    startup_info.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup_info.lpAttributeList = attr_list;

    let mut cmdline = build_command_line(program, args);
    let mut process_info: PROCESS_INFORMATION = std::mem::zeroed();

    let created = CreateProcessW(
        ptr::null(),
        cmdline.as_mut_ptr(),
        ptr::null_mut(),
        ptr::null_mut(),
        0, // do not inherit handles — ConPTY handles the pty side out-of-band
        EXTENDED_STARTUPINFO_PRESENT,
        ptr::null_mut(),
        ptr::null(),
        &mut startup_info.StartupInfo,
        &mut process_info,
    );

    DeleteProcThreadAttributeList(attr_list);

    if created == 0 {
        return Err(format!("failed to launch {program}: {}", std::io::Error::last_os_error()));
    }

    CloseHandle(process_info.hThread);
    Ok(OwnedHandle(process_info.hProcess))
}

/// Builds a mutable, NUL-terminated UTF-16 command line as `CreateProcessW`
/// requires (it may rewrite the buffer in place).
fn build_command_line(program: &str, args: &[&str]) -> Vec<u16> {
    let mut line = quote_arg(program);
    for arg in args {
        line.push(' ');
        line.push_str(&quote_arg(arg));
    }
    line.encode_utf16().chain(std::iter::once(0)).collect()
}

fn quote_arg(arg: &str) -> String {
    if arg.is_empty() || arg.contains(' ') {
        format!("\"{arg}\"")
    } else {
        arg.to_string()
    }
}

fn spawn_reader_thread(output_read: OwnedHandle, tx: Sender<Vec<u8>>) {
    thread::spawn(move || {
        // `File` over the raw handle gives us a safe, blocking `read` loop;
        // the handle is closed when this `File` (and thus `output_read`,
        // moved in) drops at thread exit.
        use std::os::windows::io::FromRawHandle;
        let handle = output_read.0;
        std::mem::forget(output_read); // ownership transferred to `file` below
        let mut file = unsafe { std::fs::File::from_raw_handle(handle as *mut _) };
        let mut buf = [0u8; 4096];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break; // receiver (GUI) gone
                    }
                }
                Err(_) => break,
            }
        }
    });
}
