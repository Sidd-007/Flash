//! The few things that differ between macOS and Windows: noticing clipboard
//! changes, keeping the computer awake, and showing files in Finder or
//! Explorer.

use std::path::Path;
use std::process::Command;

pub use imp::{KeepAwake, clipboard_change_count};

/// Opens a file with its default app.
pub fn open_path(path: &Path) -> std::io::Result<()> {
    imp::open(path)
}

/// Shows a file selected in Finder or Explorer.
pub fn reveal_path(path: &Path) -> std::io::Result<()> {
    imp::reveal(path)
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use objc2::rc::autoreleasepool;
    use objc2_app_kit::NSPasteboard;

    /// Goes up whenever anything is copied. Reading it needs no permission,
    /// unlike reading the clipboard itself.
    pub fn clipboard_change_count() -> u64 {
        autoreleasepool(|_| NSPasteboard::generalPasteboard().changeCount() as u64)
    }

    /// Holds off idle sleep while alive, via `caffeinate`, which also exits
    /// with us if we crash.
    pub struct KeepAwake(std::process::Child);

    impl KeepAwake {
        pub fn start() -> Option<Self> {
            Command::new("/usr/bin/caffeinate")
                .args(["-i", "-w", &std::process::id().to_string()])
                .spawn()
                .ok()
                .map(Self)
        }
    }

    impl Drop for KeepAwake {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    pub fn open(path: &Path) -> std::io::Result<()> {
        Command::new("open").arg(path).status().map(drop)
    }

    pub fn reveal(path: &Path) -> std::io::Result<()> {
        let mut cmd = Command::new("open");
        if !path.is_dir() {
            cmd.arg("-R");
        }
        cmd.arg(path).status().map(drop)
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::process::CommandExt;
    use std::sync::mpsc;

    pub fn clipboard_change_count() -> u64 {
        // SAFETY: takes no arguments and only reads a counter.
        unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() as u64 }
    }

    /// Holds off idle sleep while alive. Windows ties the request to the
    /// thread that made it, so a thread of its own keeps it until dropped.
    pub struct KeepAwake(#[allow(dead_code)] mpsc::Sender<()>);

    impl KeepAwake {
        pub fn start() -> Option<Self> {
            use windows_sys::Win32::System::Power::{ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState};
            let (tx, rx) = mpsc::channel::<()>();
            std::thread::spawn(move || {
                // SAFETY: plain flags; the call has no other inputs.
                unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
                // Returns once the sender is dropped.
                let _ = rx.recv();
                unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
            });
            Some(Self(tx))
        }
    }

    // Explorer's exit code doesn't mean anything, so only a failure to start counts.
    pub fn open(path: &Path) -> std::io::Result<()> {
        Command::new("explorer").arg(path).spawn().map(drop)
    }

    pub fn reveal(path: &Path) -> std::io::Result<()> {
        if path.is_dir() {
            return open(path);
        }
        // `/select,` must be glued to the quoted path, so build the argument by hand.
        Command::new("explorer").raw_arg(format!("/select,\"{}\"", path.display())).spawn().map(drop)
    }
}
