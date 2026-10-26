//! Throwaway containers for attempts and checks, run as the invoking user so that files they create
//! stay the user's.

use std::fs::{self, File};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::process::Command;

static NEXT_NAME: AtomicU64 = AtomicU64::new(0);

/// How a container's command ended.
pub struct Finished {
    pub code: Option<i32>,
    pub timed_out: bool,
    pub seconds: f64,
}

pub struct Container {
    image: String,
    network: &'static str,
    mounts: Vec<String>,
    env: Vec<String>,
}

impl Container {
    /// `network` is `host`, or `none`: for checks, and for agents in a vacuum, which reach the
    /// model through a relay's socket.
    pub fn new(image: &str, network: &'static str) -> Self {
        Container { image: image.to_owned(), network, mounts: Vec::new(), env: Vec::new() }
    }

    /// Mounts a host directory or file. Docker reads a relative source as the name of a volume, so
    /// it is made absolute first.
    pub fn mount(mut self, source: &Path, target: &str, read_only: bool) -> Self {
        let source = std::path::absolute(source).unwrap_or_else(|_| source.to_path_buf());
        let mode = if read_only { ":ro" } else { "" };
        self.mounts.push(format!("{}:{target}{mode}", source.display()));
        self
    }

    /// Sets an environment variable in the container.
    pub fn set_env(mut self, name: &str, value: &str) -> Self {
        self.env.push(format!("{name}={value}"));
        self
    }

    /// Passes an environment variable through from this process.
    pub fn pass_env(mut self, name: &str) -> Self {
        self.env.push(name.to_owned());
        self
    }

    /// Runs `command` in /work, stopping the container after `timeout`. Stderr goes to `stderr`, or
    /// with stdout to `stdout` when that is `None`.
    pub async fn run(
        &self,
        command: &[&str],
        timeout: Duration,
        stdout: &Path,
        stderr: Option<&Path>,
    ) -> Result<Finished> {
        let name = format!(
            "eidra-eval-{}-{}",
            std::process::id(),
            NEXT_NAME.fetch_add(1, Ordering::Relaxed)
        );
        let out = File::create(stdout).with_context(|| format!("creating {}", stdout.display()))?;
        let err = match stderr {
            Some(path) => {
                File::create(path).with_context(|| format!("creating {}", path.display()))?
            }
            None => out.try_clone()?,
        };
        let user = user();
        let mut cmd = Command::new("docker");
        cmd.args([
            "run",
            "--rm",
            "--init",
            "--name",
            &name,
            "--network",
            self.network,
            "--user",
            &user,
        ]);
        cmd.args(["-e", "HOME=/tmp", "-w", "/work"]);
        for mount in &self.mounts {
            cmd.args(["-v", mount]);
        }
        for var in &self.env {
            cmd.args(["-e", var]);
        }
        cmd.arg(&self.image).args(command);
        cmd.stdin(Stdio::null()).stdout(out).stderr(err).kill_on_drop(true);

        let started = Instant::now();
        let mut child = cmd.spawn().context("starting docker")?;
        let (status, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(status) => (status?, false),
            Err(_) => {
                let _ = Command::new("docker")
                    .args(["kill", &name])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await;
                (child.wait().await?, true)
            }
        };
        Ok(Finished { code: status.code(), timed_out, seconds: started.elapsed().as_secs_f64() })
    }
}

/// `uid:gid` of this process.
pub(super) fn user() -> String {
    // SAFETY: getuid and getgid cannot fail and touch no memory.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    format!("{uid}:{gid}")
}

/// Copies `from` into `to`, creating it, replacing files that are already there: it also lays a
/// solution over a workspace.
pub fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if kind.is_symlink() {
            let _ = fs::remove_file(&target);
            std::os::unix::fs::symlink(fs::read_link(entry.path())?, &target)?;
        } else {
            fs::copy(entry.path(), &target)
                .with_context(|| format!("copying {}", entry.path().display()))?;
        }
    }
    Ok(())
}
