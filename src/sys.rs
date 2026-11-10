//! What differs between Unix and Windows, in one place: the socket windows
//! attach to the server over, file modes, process groups and detaching.
//! Windows 10 and later have Unix sockets too (AF_UNIX), which uds_windows
//! wraps with the same API as std's.

use std::path::Path;
use std::process::{Child, Command};

#[cfg(unix)]
pub use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(windows)]
pub use uds_windows::{UnixListener, UnixStream};

/// Gives `path` the Unix `mode` (0o755, 0o700, ...); nothing on Windows,
/// where files are executable by name and folders are the user's own.
pub fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// Writes `text` to a file only you can read (where the system can say so).
pub fn write_private(path: &Path, text: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)?.write_all(text)
}

/// Runs `command` in a process group of its own, so that stopping it with
/// kill_group stops what it starts too (npx runs adapters as children).
pub fn own_group(command: &mut Command) -> &mut Command {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
    }
}

/// Stops a program started with own_group, and everything in its group.
pub fn kill_group(child: &mut Child) {
    #[cfg(unix)]
    {
        // SAFETY: kill with a negative pid signals that process group.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGTERM);
        }
    }
    #[cfg(windows)]
    {
        use std::process::Stdio;
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
}

/// Starts `command` on its own: neither a Ctrl-C in the terminal it came
/// from, nor the end of the ssh connection that started it, stops it.
pub fn detach(command: &mut Command) -> &mut Command {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe, and nothing else runs in the
        // child between fork and exec.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            })
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
    }
}

/// This machine's name.
pub fn hostname() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: the buffer is valid for its length; gethostname writes a
        // NUL-terminated name into it (truncated if longer).
        let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
        let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        if ok { String::from_utf8_lossy(&buf[..end]).into_owned() } else { String::new() }
    }
    #[cfg(not(unix))]
    {
        std::env::var("COMPUTERNAME").unwrap_or_default()
    }
}

/// Your login name.
pub fn user() -> String {
    std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_else(|_| "user".into())
}

/// Opened from Explorer or the Start menu, Windows gives vmac a console
/// window of its own next to the editor's; this closes it. Run from a
/// terminal, the console is the terminal's and stays (for `vmac ls` and
/// friends' output).
pub fn drop_own_console() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
        let mut ids = [0u32; 2];
        // SAFETY: the buffer is valid for its length; both calls only read
        // and detach this process's console.
        unsafe {
            if GetConsoleProcessList(ids.as_mut_ptr(), ids.len() as u32) == 1 {
                FreeConsole();
            }
        }
    }
}
