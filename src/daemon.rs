//! Foreground-free background execution (daemon mode) for `lanehub serve`.
//!
//! `lanehub serve --daemon` re-launches itself as a detached process, redirects
//! logs to `~/.local/share/lanehub/lanehub.log` and records the PID in
//! `lanehub.pid` so `lanehub stop` can terminate it later.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Everything needed to re-launch `lanehub serve` in the background.
pub struct ServeOptions {
    pub bind: SocketAddr,
    pub egress: Vec<String>,
    pub no_failover: bool,
    pub api_key: Option<String>,
    pub probe_interval: u64,
    pub config: Option<PathBuf>,
}

/// Per-user data directory for PID files, logs and user config.
pub fn data_dir() -> PathBuf {
    std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".local/share/lanehub"))
        .unwrap_or_else(|_| std::env::temp_dir().join("lanehub"))
}

pub fn pid_file() -> PathBuf {
    data_dir().join("lanehub.pid")
}

pub fn log_file() -> PathBuf {
    data_dir().join("lanehub.log")
}

fn read_pid() -> Option<u32> {
    std::fs::read_to_string(pid_file())
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Re-launch `lanehub serve ...` as a detached background process and record its PID.
pub fn spawn_detached(opts: &ServeOptions) -> Result<u32, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate executable: {e}"))?;
    let mut cmd = Command::new(&exe);
    cmd.arg("serve").arg("--bind").arg(opts.bind.to_string());
    for e in &opts.egress {
        cmd.arg("--egress").arg(e);
    }
    if opts.no_failover {
        cmd.arg("--no-failover");
    }
    if let Some(k) = &opts.api_key {
        cmd.arg("--api-key").arg(k);
    }
    cmd.arg("--probe-interval").arg(opts.probe_interval.to_string());
    if let Some(c) = &opts.config {
        cmd.arg("--config").arg(c);
    }

    std::fs::create_dir_all(data_dir())
        .map_err(|e| format!("cannot create {}: {e}", data_dir().display()))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file())
        .map_err(|e| format!("cannot open log file: {e}"))?;
    let log_err = log
        .try_clone()
        .map_err(|e| format!("cannot duplicate log handle: {e}"))?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));


    let child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn background process: {e}"))?;
    let pid = child.id();
    std::fs::write(pid_file(), pid.to_string())
        .map_err(|e| format!("cannot write pid file: {e}"))?;
    Ok(pid)
}

/// Stop a previously spawned background instance via its PID file.
pub fn stop() -> Result<(), String> {
    let pid =
        read_pid().ok_or_else(|| "no PID file found — is the background instance running?".to_string())?;
    let success = Command::new("kill")
        .arg(pid.to_string())
        .status()
        .map_err(|e| format!("failed to run kill: {e}"))?
        .success();

    if success {
        let _ = std::fs::remove_file(pid_file());
        println!("stopped background lanehub (pid {pid})");
        Ok(())
    } else {
        Err(format!(
            "failed to terminate pid {pid} (process may already be gone)"
        ))
    }
}
