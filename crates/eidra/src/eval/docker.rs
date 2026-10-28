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

    /// The `docker run` arguments for `command`, in one place so that tests can read them: a
    /// container's network, and what it may reach, is what `eidra eval vacuum-check` checks.
    pub(super) fn args(&self, name: &str, command: &[&str]) -> Vec<String> {
        let mut args: Vec<String> = ["run", "--rm", "--init"].map(String::from).to_vec();
        args.extend(["--name".into(), name.to_owned()]);
        args.extend(["--network".into(), self.network.to_owned()]);
        args.extend(["--user".into(), user()]);
        args.extend(["-e".into(), "HOME=/tmp".into(), "-w".into(), "/work".into()]);
        for mount in &self.mounts {
            args.extend(["-v".into(), mount.clone()]);
        }
        for var in &self.env {
            args.extend(["-e".into(), var.clone()]);
        }
        args.push(self.image.clone());
        args.extend(command.iter().map(|arg| (*arg).to_owned()));
        args
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
        let mut cmd = Command::new("docker");
        cmd.args(self.args(&name, command));
        cmd.stdin(Stdio::null()).stdout(out).stderr(err).kill_on_drop(true);

        let started = Instant::now();
        let mut child = cmd.spawn().context("starting docker")?;
        let mut running = Running { name: &name, ended: false };
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
        running.ended = true;
        Ok(Finished { code: status.code(), timed_out, seconds: started.elapsed().as_secs_f64() })
    }
}

/// A container being run. Dropped before it ends, as when the run stops waiting on an attempt, it
/// kills the container: killing `docker run` alone would leave it running.
struct Running<'a> {
    name: &'a str,
    ended: bool,
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        if !self.ended {
            let _ = std::process::Command::new("docker")
                .args(["kill", self.name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A container with no network gets none: the flag is what the vacuum rests on, so it is
    /// worth pinning here as well as in the live check (`eidra eval vacuum-check`).
    #[test]
    fn a_container_passes_its_network_to_docker() {
        let args = Container::new("python:3.13-slim", "none").args("c", &["true"]);
        assert!(args.windows(2).any(|pair| pair == ["--network", "none"]));
        assert_eq!(args.last().unwrap(), "true");
        assert!(args.contains(&"python:3.13-slim".to_owned()));
    }

    #[test]
    fn mounts_and_variables_reach_the_command() {
        let args = Container::new("img", "none")
            .mount(Path::new("/tmp/work"), "/work", false)
            .mount(Path::new("/tmp/check"), "/check", true)
            .set_env("A", "1")
            .pass_env("B")
            .args("c", &["sh", "-c", "echo hi"]);
        assert!(args.windows(2).any(|pair| pair == ["-v", "/tmp/work:/work"]));
        assert!(args.windows(2).any(|pair| pair == ["-v", "/tmp/check:/check:ro"]));
        assert!(args.windows(2).any(|pair| pair == ["-e", "A=1"]));
        assert!(args.windows(2).any(|pair| pair == ["-e", "B"]));
        // Everything after the image is the command, untouched.
        let image = args.iter().position(|arg| arg == "img").unwrap();
        assert_eq!(&args[image + 1..], ["sh", "-c", "echo hi"]);
    }
}
