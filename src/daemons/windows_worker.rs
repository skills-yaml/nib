//! Windows durable-worker creation without ambient handle inheritance.

#![cfg(windows)]

use std::ffi::OsStr;
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList,
    TerminateProcess, UpdateProcThreadAttribute, CREATE_NO_WINDOW, DETACHED_PROCESS,
    EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

const MAX_COMMAND_LINE_UNITS: usize = 32_767;
const NULL_DEVICE: [u16; 4] = [b'N' as u16, b'U' as u16, b'L' as u16, 0];

pub(super) fn spawn_detached_worker(
    executable: &Path,
    daemon_dir: &Path,
    task_id: &str,
    lease_token: &str,
    current_dir: &Path,
) -> io::Result<u32> {
    let application = nul_terminated(executable.as_os_str(), "worker executable")?;
    let current_dir = nul_terminated(current_dir.as_os_str(), "worker current directory")?;
    let mut command_line = Vec::new();
    for argument in [
        executable.as_os_str(),
        OsStr::new("task-worker"),
        OsStr::new("--daemon-dir"),
        daemon_dir.as_os_str(),
        OsStr::new("--task-id"),
        OsStr::new(task_id),
        OsStr::new("--lease-token"),
        OsStr::new(lease_token),
    ] {
        append_quoted_argument(&mut command_line, argument)?;
    }
    if command_line.len() >= MAX_COMMAND_LINE_UNITS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "durable worker command line exceeds the Windows limit",
        ));
    }
    command_line.push(0);

    let null_stdio = NullStdio::open()?;
    let inherited_handles = null_stdio.raw_handles();
    let mut attributes = ProcThreadAttributeList::with_handle_list(&inherited_handles)?;
    let mut startup = STARTUPINFOEXW {
        lpAttributeList: attributes.as_ptr(),
        ..STARTUPINFOEXW::default()
    };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = inherited_handles[0];
    startup.StartupInfo.hStdOutput = inherited_handles[1];
    startup.StartupInfo.hStdError = inherited_handles[2];
    let mut process = PROCESS_INFORMATION::default();
    // Every pointer remains valid for the call, and the explicit handle list is
    // the complete inheritance boundary for this child.
    let created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            1,
            DETACHED_PROCESS | CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT,
            ptr::null(),
            current_dir.as_ptr(),
            (&raw const startup).cast(),
            &mut process,
        )
    };
    if created == 0 {
        return Err(io::Error::last_os_error());
    }
    if process.hProcess.is_null() || process.hThread.is_null() || process.dwProcessId == 0 {
        unsafe {
            if !process.hProcess.is_null() {
                let _ = TerminateProcess(process.hProcess, 1);
                drop(OwnedHandle::from_raw_handle(process.hProcess.cast()));
            }
            if !process.hThread.is_null() {
                drop(OwnedHandle::from_raw_handle(process.hThread.cast()));
            }
        }
        return Err(io::Error::other(
            "Windows created a durable worker without complete process identity",
        ));
    }

    let process_id = process.dwProcessId;
    unsafe {
        drop(OwnedHandle::from_raw_handle(process.hThread.cast()));
        drop(OwnedHandle::from_raw_handle(process.hProcess.cast()));
    }
    Ok(process_id)
}

struct NullStdio {
    stdin: OwnedHandle,
    stdout: OwnedHandle,
    stderr: OwnedHandle,
}

impl NullStdio {
    fn open() -> io::Result<Self> {
        Ok(Self {
            stdin: open_inheritable_null(GENERIC_READ)?,
            stdout: open_inheritable_null(GENERIC_WRITE)?,
            stderr: open_inheritable_null(GENERIC_WRITE)?,
        })
    }

    fn raw_handles(&self) -> [HANDLE; 3] {
        [
            self.stdin.as_raw_handle() as HANDLE,
            self.stdout.as_raw_handle() as HANDLE,
            self.stderr.as_raw_handle() as HANDLE,
        ]
    }
}

fn open_inheritable_null(access: u32) -> io::Result<OwnedHandle> {
    let security = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 1,
    };
    let handle = unsafe {
        CreateFileW(
            NULL_DEVICE.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &security,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle.cast()) })
}

struct ProcThreadAttributeList {
    storage: Vec<usize>,
    handles: Box<[HANDLE]>,
}

impl ProcThreadAttributeList {
    fn with_handle_list(handles: &[HANDLE]) -> io::Result<Self> {
        let mut bytes = 0usize;
        unsafe {
            let _ = InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut bytes);
        }
        if bytes == 0 {
            return Err(io::Error::last_os_error());
        }
        let words = bytes.div_ceil(size_of::<usize>());
        let mut storage = vec![0usize; words];
        if unsafe {
            InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), 1, 0, &mut bytes)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut list = Self {
            storage,
            handles: handles.to_vec().into_boxed_slice(),
        };
        let updated = unsafe {
            UpdateProcThreadAttribute(
                list.as_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                list.handles.as_ptr().cast(),
                std::mem::size_of_val(list.handles.as_ref()),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if updated == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(list)
    }

    fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.storage.as_mut_ptr().cast()
    }
}

impl Drop for ProcThreadAttributeList {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.as_ptr());
        }
    }
}

fn nul_terminated(value: &OsStr, label: &str) -> io::Result<Vec<u16>> {
    let mut wide = Vec::new();
    for unit in value.encode_wide() {
        if unit == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{label} contains a NUL code unit"),
            ));
        }
        if wide.len() + 1 >= MAX_COMMAND_LINE_UNITS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{label} exceeds the Windows UTF-16 limit"),
            ));
        }
        wide.push(unit);
    }
    wide.push(0);
    Ok(wide)
}

fn append_quoted_argument(command_line: &mut Vec<u16>, argument: &OsStr) -> io::Result<()> {
    let mut units = Vec::new();
    for unit in argument.encode_wide() {
        if unit == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "durable worker argument contains a NUL code unit",
            ));
        }
        if units.len() >= MAX_COMMAND_LINE_UNITS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "durable worker argument exceeds the Windows UTF-16 limit",
            ));
        }
        units.push(unit);
    }
    let mut quoted = Vec::with_capacity(units.len().saturating_add(2));
    quoted.push(u16::from(b'"'));

    let mut backslashes = 0usize;
    for unit in units {
        if unit == u16::from(b'\\') {
            backslashes += 1;
            continue;
        }
        if unit == u16::from(b'"') {
            push_backslashes(&mut quoted, backslashes * 2 + 1);
            quoted.push(unit);
        } else {
            push_backslashes(&mut quoted, backslashes);
            quoted.push(unit);
        }
        backslashes = 0;
    }
    push_backslashes(&mut quoted, backslashes * 2);
    quoted.push(u16::from(b'"'));

    let separator = usize::from(!command_line.is_empty());
    if command_line
        .len()
        .checked_add(separator)
        .and_then(|length| length.checked_add(quoted.len()))
        .is_none_or(|length| length >= MAX_COMMAND_LINE_UNITS)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "durable worker command line exceeds the Windows limit",
        ));
    }
    if separator != 0 {
        command_line.push(u16::from(b' '));
    }
    command_line.extend(quoted);
    Ok(())
}

fn push_backslashes(command_line: &mut Vec<u16>, count: usize) {
    command_line.extend(std::iter::repeat_n(u16::from(b'\\'), count));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_arguments_preserve_spaces_quotes_and_trailing_backslashes() {
        let mut command_line = Vec::new();
        append_quoted_argument(&mut command_line, OsStr::new(r#"say "hi""#))
            .expect("quote embedded quotes");
        let rendered = String::from_utf16(&command_line).expect("ASCII fixture");
        assert_eq!(rendered, r#""say \"hi\"""#);

        command_line.clear();
        append_quoted_argument(&mut command_line, OsStr::new(r#"C:\path with space\"#))
            .expect("quote trailing backslash");
        let rendered = String::from_utf16(&command_line).expect("ASCII fixture");
        assert_eq!(rendered, r#""C:\path with space\\""#);
    }
}
