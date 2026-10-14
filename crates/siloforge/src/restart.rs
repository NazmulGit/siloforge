//! `POST /api/v2/system/restart` (v2.1): re-launch the server in seconds.
//!
//! The serve loop shuts down gracefully (jobs drained, listeners closed, DB closed), releases the
//! instance lock, then calls [`relaunch`] and returns, so the process exits right after.
//!
//! * Under the `siloforge-keepalive` scheduled task (our parent is a headless `conhost.exe`, or
//!   `SILOFORGE_RESTART_MODE=task`): a detached `cmd /c ping -n 3 127.0.0.1 >nul & schtasks /run
//!   /tn <task>` waits about 2 s (so this process has exited and the task is idle) and starts the
//!   task again. `ping` is used instead of `timeout /t 2` because `timeout` aborts at once when
//!   it has no console input, which is the case for a detached child.
//! * Otherwise (`exec`): the same executable is started again with the same arguments,
//!   detached, after the lock and the ports were released.
//! * `SILOFORGE_RESTART_MODE=none`: just exit (a supervisor restarts us).

use std::process::{Command, Stdio};

/// How the process comes back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    Task(String),
    Exec,
    None,
}

/// Default task name (scripts/install.ps1 creates it).
pub const DEFAULT_TASK: &str = "siloforge-keepalive";

/// Decide from `SILOFORGE_RESTART_MODE` / `SILOFORGE_RESTART_TASK` and whether our parent looks
/// like the scheduled task's headless console host.
pub fn plan_from(mode: Option<&str>, task: Option<&str>, parent_is_task_host: bool) -> Plan {
    let task = task
        .map(str::trim)
        .filter(|t| {
            !t.is_empty()
                && t.len() <= 200
                && t.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_. \\".contains(c))
        })
        .unwrap_or(DEFAULT_TASK)
        .to_string();
    match mode.map(|m| m.trim().to_ascii_lowercase()).as_deref() {
        Some("task") => Plan::Task(task),
        Some("exec") => Plan::Exec,
        Some("none") => Plan::None,
        _ if parent_is_task_host => Plan::Task(task),
        _ => Plan::Exec,
    }
}

/// True when the parent process is `conhost.exe ... --headless` (how the keepalive task runs us).
fn parent_is_task_host() -> bool {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
    let Ok(me) = sysinfo::get_current_pid() else {
        return false;
    };
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        false,
        ProcessRefreshKind::nothing().with_cmd(sysinfo::UpdateKind::Always),
    );
    let Some(parent) = sys.process(me).and_then(|p| p.parent()) else {
        return false;
    };
    let Some(pp) = sys.process(parent) else {
        return false;
    };
    let name = pp.name().to_string_lossy().to_ascii_lowercase();
    let headless = pp
        .cmd()
        .iter()
        .any(|a| a.to_string_lossy().eq_ignore_ascii_case("--headless"));
    name == "conhost.exe" && headless
}

pub fn current_plan() -> Plan {
    let mode = std::env::var("SILOFORGE_RESTART_MODE").ok();
    let task = std::env::var("SILOFORGE_RESTART_TASK").ok();
    let host = mode.is_none() && parent_is_task_host();
    plan_from(mode.as_deref(), task.as_deref(), host)
}

#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB);
}

#[cfg(not(windows))]
fn detach(_cmd: &mut Command) {}

#[cfg(windows)]
fn without_breakaway(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0000_0200 | 0x0800_0000);
}

#[cfg(not(windows))]
fn without_breakaway(_cmd: &mut Command) {}

fn spawn(mut cmd: Command) -> std::io::Result<()> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut cmd);
    match cmd.spawn() {
        Ok(_) => Ok(()),
        // a job object that forbids breakaway: try again inside it
        Err(_) => {
            without_breakaway(&mut cmd);
            cmd.spawn().map(|_| ())
        }
    }
}

/// Starts the replacement process (see the module docs). Call after the lock was released.
pub fn relaunch(plan: &Plan) {
    let res = match plan {
        Plan::Task(task) => {
            let mut c = Command::new("cmd");
            c.args([
                "/c",
                &format!("ping -n 3 127.0.0.1 >nul & schtasks /run /tn \"{task}\""),
            ]);
            spawn(c)
        }
        Plan::Exec => match std::env::current_exe() {
            Ok(exe) => {
                let mut c = Command::new(exe);
                c.args(std::env::args_os().skip(1));
                if let Ok(d) = std::env::current_dir() {
                    c.current_dir(d);
                }
                spawn(c)
            }
            Err(e) => Err(e),
        },
        Plan::None => Ok(()),
    };
    match res {
        Ok(()) => tracing::warn!(?plan, "restart: replacement launched, exiting"),
        Err(e) => tracing::error!(?plan, error = %e, "restart: could not launch the replacement"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_selection() {
        assert_eq!(plan_from(None, None, false), Plan::Exec);
        assert_eq!(plan_from(None, None, true), Plan::Task(DEFAULT_TASK.into()));
        assert_eq!(
            plan_from(Some("TASK"), Some("my-task"), false),
            Plan::Task("my-task".into())
        );
        assert_eq!(plan_from(Some("exec"), None, true), Plan::Exec);
        assert_eq!(plan_from(Some("none"), None, true), Plan::None);
        // hostile task names fall back to the default (no quoting games in cmd)
        assert_eq!(
            plan_from(Some("task"), Some("x\" & calc & \""), false),
            Plan::Task(DEFAULT_TASK.into())
        );
        assert_eq!(plan_from(Some("weird"), None, false), Plan::Exec);
    }

    #[test]
    fn task_host_detection_never_panics() {
        let _ = parent_is_task_host();
        let _ = current_plan();
    }
}
