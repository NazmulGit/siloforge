//! Single-instance guard for a data directory (R2).
//!
//! `serve` (and `restore`) hold an exclusive OS lock on `$HOME/siloforge.lock` for their whole
//! lifetime (`File::try_lock`: `LockFileEx` on Windows, `flock` elsewhere). The OS releases it when
//! the process dies, so a crash never leaves a stale lock. The pid is written to
//! `$HOME/siloforge.pid` for humans and older tools; it is only removed by the process that wrote it.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

const PID_FILE: &str = "siloforge.pid";
const LOCK_FILE: &str = "siloforge.lock";

/// Why the guard could not be taken.
#[derive(Debug)]
pub enum AcquireError {
    /// Another process holds the lock (pid when known).
    Running(Option<u32>),
    Io(std::io::Error),
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running(Some(pid)) => write!(
                f,
                "another SiloForge process (pid {pid}) is using this data directory"
            ),
            Self::Running(None) => {
                write!(f, "another SiloForge process is using this data directory")
            }
            Self::Io(e) => write!(f, "instance lock: {e}"),
        }
    }
}

impl std::error::Error for AcquireError {}

impl From<std::io::Error> for AcquireError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Held while the process owns the data directory; releases the lock on drop.
pub struct InstanceGuard {
    pid_path: PathBuf,
    _lock: File,
}

fn open_lock(home: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.join(LOCK_FILE))
}

fn read_pid(home: &Path) -> Option<u32> {
    std::fs::read_to_string(home.join(PID_FILE))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn pid_alive(pid: u32) -> bool {
    let mut sys = System::new();
    let p = Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[p]),
        true,
        ProcessRefreshKind::nothing(),
    );
    sys.process(p).is_some()
}

impl InstanceGuard {
    /// Takes the exclusive lock, then records our pid. Fails fast when another process holds it.
    // File::try_lock is std since 1.89; the toolchain here is newer than the declared MSRV.
    #[allow(clippy::incompatible_msrv)]
    pub fn acquire(home: &Path) -> Result<Self, AcquireError> {
        std::fs::create_dir_all(home)?;
        let lock = open_lock(home)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(AcquireError::Running(read_pid(home))),
            Err(TryLockError::Error(e)) => return Err(AcquireError::Io(e)),
        }
        let pid_path = home.join(PID_FILE);
        let mut f = File::create(&pid_path)?;
        f.write_all(std::process::id().to_string().as_bytes())?;
        Ok(Self {
            pid_path,
            _lock: lock,
        })
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        // Only remove a pid file that still names us (never another instance's).
        let ours = std::fs::read_to_string(&self.pid_path)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            == Some(std::process::id());
        if ours {
            let _ = std::fs::remove_file(&self.pid_path);
        }
        // The lock itself is released when `_lock` is closed; the lock file stays (harmless).
    }
}

/// Pid of a live SiloForge process using `home`, if any. The OS lock is authoritative; a pid file
/// without the lock (an older binary) counts only when that pid is alive.
#[allow(clippy::incompatible_msrv)]
pub fn running_instance(home: &Path) -> Option<u32> {
    if let Ok(f) = OpenOptions::new()
        .read(true)
        .write(true)
        .open(home.join(LOCK_FILE))
    {
        match f.try_lock() {
            Err(TryLockError::WouldBlock) => return Some(read_pid(home).unwrap_or(0)),
            Ok(()) => {
                let _ = f.unlock();
            }
            Err(TryLockError::Error(_)) => {}
        }
    }
    let pid = read_pid(home)?;
    (pid != std::process::id() && pid_alive(pid)).then_some(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_exclusive_and_pid_file_is_owned() {
        let dir = std::env::temp_dir().join(format!("sf-ops-inst-{}", sf_core::new_id()));
        assert_eq!(running_instance(&dir), None);
        {
            let _g = InstanceGuard::acquire(&dir).unwrap();
            assert_eq!(running_instance(&dir), Some(std::process::id()));
            // a second acquire fails fast and does not touch the first one's pid file
            let second = InstanceGuard::acquire(&dir);
            assert!(matches!(
                second,
                Err(AcquireError::Running(Some(p))) if p == std::process::id()
            ));
            drop(second);
            assert!(dir.join(PID_FILE).exists());
            assert_eq!(running_instance(&dir), Some(std::process::id()));
        }
        assert_eq!(running_instance(&dir), None);
        assert!(!dir.join(PID_FILE).exists());
        // a stale pid file of a dead process is ignored and the lock can be taken
        std::fs::write(dir.join(PID_FILE), "4294967").unwrap();
        assert_eq!(running_instance(&dir), None);
        let g = InstanceGuard::acquire(&dir).unwrap();
        // someone else rewrote the pid file: our drop must not delete it
        std::fs::write(dir.join(PID_FILE), "4294967").unwrap();
        drop(g);
        assert!(dir.join(PID_FILE).exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
