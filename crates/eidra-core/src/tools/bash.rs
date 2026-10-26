//! `bash`, `bash_output` and `bash_kill` (docs/DESIGN.md §6).
//!
//! Every command is a fresh `bash -c`, so a wedged shell cannot outlive it, but the working
//! directory and exported variables carry over to the next command, so `cd` and
//! `source .venv/bin/activate` stick. Each command gets its own process group, which a timeout
//! stops as a whole.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStdout, Command};
use tokio::time::Instant;

use crate::confirm::{ConfirmPolicy, Verdict};
use crate::event::{Approval, Decision, Frontend};
use crate::text::clean_terminal_output;

const DEFAULT_TIMEOUT_S: u64 = 120;
const MAX_TIMEOUT_S: u64 = 600;
/// Output kept per command; past this the middle is dropped.
const CAPTURE_LIMIT: usize = 4 << 20;
/// Output kept per background job; past this the oldest goes.
const JOB_LOG_LIMIT: usize = 1 << 20;

/// Runs the command, then saves the working directory and environment as the shell exits, even
/// through `exit`. The command arrives in an environment variable, so it needs no quoting, and
/// `eval` runs it in this shell, so its `cd` and `export` are what gets saved.
const WRAPPER: &str = r#"exec 2>&1
__eidra_save() { __eidra_status=$?; pwd > "$EIDRA_STATE/cwd"; env -0 > "$EIDRA_STATE/env"; exit "$__eidra_status"; }
trap __eidra_save EXIT
__eidra_command=$EIDRA_COMMAND
unset EIDRA_COMMAND
eval "$__eidra_command"
"#;

/// Set for every command, so that nothing waits for a terminal.
const NON_INTERACTIVE: &[(&str, &str)] = &[
    ("TERM", "dumb"),
    ("NO_COLOR", "1"),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("MANPAGER", "cat"),
    ("GIT_EDITOR", "true"),
    ("EDITOR", "true"),
    ("VISUAL", "true"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("PIP_NO_INPUT", "1"),
    ("DEBIAN_FRONTEND", "noninteractive"),
    ("PYTHONUNBUFFERED", "1"),
];

/// Variables from a saved environment that are not carried to the next command.
const NOT_CARRIED: [&str; 4] = ["EIDRA_STATE", "EIDRA_COMMAND", "_", "SHLVL"];

pub struct Shell {
    cwd: PathBuf,
    env: BTreeMap<String, String>,
    workspace: PathBuf,
    state: PathBuf,
    jobs: BTreeMap<u64, Job>,
    next_job: u64,
}

struct Job {
    pgid: i32,
    log: Arc<Mutex<JobLog>>,
    /// Bytes of output already shown.
    shown: usize,
}

#[derive(Default)]
struct JobLog {
    bytes: Vec<u8>,
    /// Output dropped from the front of `bytes`.
    dropped: usize,
    exit: Option<String>,
}

impl Shell {
    /// `state` is a directory for the shell's saved state. `secret_env` names a variable commands
    /// must not see: the API key.
    pub fn new(workspace: &Path, state: &Path, secret_env: Option<&str>) -> std::io::Result<Self> {
        fs::create_dir_all(state)?;
        let mut env: BTreeMap<String, String> = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect();
        if let Some(name) = secret_env {
            env.remove(name);
        }
        for (key, value) in NON_INTERACTIVE {
            env.insert((*key).into(), (*value).into());
        }
        Ok(Shell {
            cwd: workspace.to_path_buf(),
            env,
            workspace: workspace.to_path_buf(),
            state: state.to_path_buf(),
            jobs: BTreeMap::new(),
            next_job: 1,
        })
    }

    pub async fn run<F: Frontend>(
        &mut self,
        command: &str,
        timeout_s: Option<u64>,
        background: bool,
        confirm: &mut ConfirmPolicy,
        frontend: &mut F,
    ) -> Result<String, String> {
        if command.trim().is_empty() {
            return Err("command is empty".into());
        }
        if let Verdict::Ask(reasons) = confirm.check(command) {
            let approval = Approval { command: command.to_owned(), reasons };
            match frontend.approve(&approval).await {
                Decision::Once => {}
                Decision::Session => confirm.allow_for_session(command),
                Decision::Deny { note } => {
                    return Err(format!(
                        "not run: this command needs the user's approval ({}). {}",
                        approval.reasons.join("; "),
                        note.as_deref().unwrap_or("The user declined it.")
                    ));
                }
            }
        }
        if !self.cwd.is_dir() {
            self.cwd = self.workspace.clone();
        }
        if background {
            return self.start_job(command);
        }
        let timeout =
            Duration::from_secs(timeout_s.unwrap_or(DEFAULT_TIMEOUT_S).clamp(1, MAX_TIMEOUT_S));
        self.run_foreground(command, timeout).await
    }

    fn command(&self, command: &str, state: &Path) -> Command {
        let mut cmd = std::process::Command::new("bash");
        cmd.arg("-c")
            .arg(WRAPPER)
            .current_dir(&self.cwd)
            .env_clear()
            .envs(&self.env)
            .env("EIDRA_COMMAND", command)
            .env("EIDRA_STATE", state)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        let mut cmd = Command::from(cmd);
        cmd.kill_on_drop(true);
        cmd
    }

    async fn run_foreground(&mut self, command: &str, timeout: Duration) -> Result<String, String> {
        let _ = fs::remove_file(self.state.join("cwd"));
        let _ = fs::remove_file(self.state.join("env"));
        let mut child = self
            .command(command, &self.state)
            .spawn()
            .map_err(|e| format!("could not start bash: {e}"))?;
        let pgid = child.id().map(|pid| pid as i32);
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let mut capture = Capture::default();

        let status =
            match tokio::time::timeout(timeout, collect(&mut child, &mut stdout, &mut capture))
                .await
            {
                Ok(Ok(status)) => {
                    self.load_state();
                    describe_exit(status)
                }
                Ok(Err(e)) => return Err(format!("reading the command's output failed: {e}")),
                Err(_) => {
                    if let Some(pgid) = pgid {
                        stop_group(pgid).await;
                    }
                    let _ = child.wait().await;
                    Some(format!(
                        "timed out after {} s; the command and everything it started were stopped",
                        timeout.as_secs()
                    ))
                }
            };
        let output = clean_terminal_output(&capture.into_string());
        let output = output.trim_end();
        Ok(match (output.is_empty(), status) {
            (true, None) => "(no output)".into(),
            (true, Some(status)) => format!("[{status}]"),
            (false, None) => output.to_owned(),
            (false, Some(status)) => format!("{output}\n[{status}]"),
        })
    }

    /// Reads the directory and environment the last command left behind.
    fn load_state(&mut self) {
        if let Ok(cwd) = fs::read_to_string(self.state.join("cwd")) {
            let cwd = cwd.trim_end_matches('\n');
            if !cwd.is_empty() {
                self.cwd = PathBuf::from(cwd);
            }
        }
        if let Ok(raw) = fs::read(self.state.join("env")) {
            let env: BTreeMap<String, String> = raw
                .split(|&b| b == 0)
                .filter_map(|entry| {
                    let (key, value) = std::str::from_utf8(entry).ok()?.split_once('=')?;
                    (!key.is_empty() && !NOT_CARRIED.contains(&key))
                        .then(|| (key.to_owned(), value.to_owned()))
                })
                .collect();
            if !env.is_empty() {
                self.env = env;
            }
        }
    }

    fn start_job(&mut self, command: &str) -> Result<String, String> {
        let id = self.next_job;
        self.next_job += 1;
        let state = self.state.join(format!("job-{id}"));
        fs::create_dir_all(&state).map_err(|e| format!("creating {}: {e}", state.display()))?;
        let mut child = self
            .command(command, &state)
            .spawn()
            .map_err(|e| format!("could not start bash: {e}"))?;
        let pgid = child.id().map(|pid| pid as i32).ok_or("the job exited at once")?;
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let log = Arc::new(Mutex::new(JobLog::default()));
        let task_log = Arc::clone(&log);
        tokio::spawn(async move {
            let mut buf = vec![0u8; 16 << 10];
            loop {
                match stdout.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => task_log.lock().unwrap().push(&buf[..n]),
                }
            }
            let exit = match child.wait().await {
                Ok(status) => describe_exit(status).unwrap_or_else(|| "exit code 0".into()),
                Err(e) => e.to_string(),
            };
            task_log.lock().unwrap().exit = Some(exit);
        });
        self.jobs.insert(id, Job { pgid, log, shown: 0 });
        Ok(format!(
            "Started job {id} in the background. Read its output with bash_output (id {id}), and stop it with bash_kill."
        ))
    }

    pub fn job_output(&mut self, id: u64) -> Result<String, String> {
        let job = self.jobs.get_mut(&id).ok_or_else(|| format!("there is no job {id}"))?;
        let log = job.log.lock().unwrap();
        let total = log.dropped + log.bytes.len();
        let mut text = String::new();
        if job.shown < log.dropped {
            text.push_str(&format!(
                "[… {} bytes of output were dropped before you read them …]\n",
                log.dropped - job.shown
            ));
        }
        let start = job.shown.saturating_sub(log.dropped);
        text.push_str(&String::from_utf8_lossy(&log.bytes[start..]));
        job.shown = total;
        let status = match &log.exit {
            Some(exit) => format!("job {id} has ended ({exit})"),
            None => format!("job {id} is running"),
        };
        let text = clean_terminal_output(&text);
        let text = text.trim_end();
        Ok(if text.is_empty() {
            format!("[{status}; no new output]")
        } else {
            format!("[{status}]\n{text}")
        })
    }

    pub async fn kill_job(&mut self, id: u64) -> Result<String, String> {
        let job = self.jobs.get(&id).ok_or_else(|| format!("there is no job {id}"))?;
        if job.log.lock().unwrap().exit.is_some() {
            return Ok(format!("Job {id} had already ended."));
        }
        stop_group(job.pgid).await;
        Ok(format!("Stopped job {id}."))
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        for job in self.jobs.values() {
            // SAFETY: killpg only sends a signal, to a process group this shell created.
            unsafe { libc::killpg(job.pgid, libc::SIGKILL) };
        }
    }
}

/// Reads a command's output until it exits. Processes the command left running can keep the pipe
/// open, so once bash has exited, reading stops after a moment.
async fn collect(
    child: &mut Child,
    stdout: &mut ChildStdout,
    capture: &mut Capture,
) -> std::io::Result<ExitStatus> {
    let mut buf = vec![0u8; 64 << 10];
    let mut status = None;
    let mut eof = false;
    let mut drain_until: Option<Instant> = None;
    loop {
        tokio::select! {
            read = stdout.read(&mut buf), if !eof => match read? {
                0 => eof = true,
                n => capture.push(&buf[..n]),
            },
            exit = child.wait(), if status.is_none() => {
                status = Some(exit?);
                drain_until = Some(Instant::now() + Duration::from_millis(200));
            }
            _ = tokio::time::sleep_until(drain_until.unwrap_or_else(Instant::now)), if drain_until.is_some() && !eof => break,
        }
        if eof && status.is_some() {
            break;
        }
    }
    Ok(status.expect("the loop ends only after the command exits"))
}

/// SIGTERM to the whole group, then SIGKILL if it is still there after a moment.
async fn stop_group(pgid: i32) {
    // SAFETY: killpg only sends signals, to a process group this shell created.
    unsafe { libc::killpg(pgid, libc::SIGTERM) };
    for _ in 0..15 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        // SAFETY: as above; signal 0 only checks that the group exists.
        if unsafe { libc::killpg(pgid, 0) } != 0 {
            return;
        }
    }
    // SAFETY: as above.
    unsafe { libc::killpg(pgid, libc::SIGKILL) };
}

/// `None` for success, otherwise how the command ended.
fn describe_exit(status: ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt as _;
    match (status.code(), status.signal()) {
        (Some(0), _) => None,
        (Some(code), _) => Some(format!("exit code {code}")),
        (None, Some(signal)) => Some(format!("killed by signal {signal}")),
        (None, None) => Some("ended abnormally".into()),
    }
}

/// A command's output, with the middle dropped past `CAPTURE_LIMIT`.
#[derive(Default)]
struct Capture {
    head: Vec<u8>,
    tail: Vec<u8>,
    dropped: usize,
}

impl Capture {
    fn push(&mut self, mut bytes: &[u8]) {
        let half = CAPTURE_LIMIT / 2;
        if self.head.len() < half {
            let take = (half - self.head.len()).min(bytes.len());
            self.head.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
        }
        self.tail.extend_from_slice(bytes);
        if self.tail.len() > half {
            let excess = self.tail.len() - half;
            self.tail.drain(..excess);
            self.dropped += excess;
        }
    }

    fn into_string(self) -> String {
        let mut text = String::from_utf8_lossy(&self.head).into_owned();
        if self.dropped > 0 {
            text.push_str(&format!("\n[… {} bytes of output dropped …]\n", self.dropped));
        }
        text.push_str(&String::from_utf8_lossy(&self.tail));
        text
    }
}

impl JobLog {
    fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
        if self.bytes.len() > JOB_LOG_LIMIT {
            let excess = self.bytes.len() - JOB_LOG_LIMIT / 2;
            self.bytes.drain(..excess);
            self.dropped += excess;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;

    /// A frontend that approves nothing and records nothing.
    struct Headless;

    impl Frontend for Headless {
        fn event(&mut self, _: Event) {}
        async fn approve(&mut self, _: &Approval) -> Decision {
            Decision::Deny { note: Some("Nobody can approve it here.".into()) }
        }
    }

    /// A shell in a fresh directory. It hides HOME as if it were the API key, since HOME is set
    /// wherever the tests run.
    fn shell(name: &str) -> (Shell, PathBuf) {
        let dir = std::env::temp_dir().join(format!("eidra-bash-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        (Shell::new(&dir, &dir.join(".state"), Some("HOME")).unwrap(), dir)
    }

    async fn run(shell: &mut Shell, command: &str) -> String {
        let mut policy = ConfirmPolicy::new::<&str>(&[]);
        shell.run(command, Some(10), false, &mut policy, &mut Headless).await.unwrap()
    }

    #[tokio::test]
    async fn carries_the_directory_and_exports_and_reports_exit_codes() {
        let (mut shell, _dir) = shell("state");
        assert_eq!(run(&mut shell, "cd sub && export GREETING=hi").await, "(no output)");
        assert!(run(&mut shell, "pwd").await.ends_with("/sub"));
        assert_eq!(run(&mut shell, "echo $GREETING; echo oops >&2").await, "hi\noops");
        assert_eq!(run(&mut shell, "exit 3").await, "[exit code 3]");
        assert_eq!(run(&mut shell, "echo $PAGER $TERM").await, "cat dumb");
    }

    #[tokio::test]
    async fn times_out_and_stops_what_the_command_started() {
        let (mut shell, _dir) = shell("timeout");
        let mut policy = ConfirmPolicy::new::<&str>(&[]);
        let out = shell
            .run("echo start; sleep 30 & sleep 30", Some(1), false, &mut policy, &mut Headless)
            .await
            .unwrap();
        assert!(out.starts_with("start\n[timed out after 1 s"), "{out}");
    }

    #[tokio::test]
    async fn does_not_wait_for_processes_left_holding_the_output() {
        let (mut shell, _dir) = shell("orphan");
        let started = std::time::Instant::now();
        assert_eq!(run(&mut shell, "(sleep 5 &) ; echo done").await, "done");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn runs_background_jobs() {
        let (mut shell, _dir) = shell("jobs");
        let mut policy = ConfirmPolicy::new::<&str>(&[]);
        let started = shell
            .run("echo ready; sleep 30", None, true, &mut policy, &mut Headless)
            .await
            .unwrap();
        assert!(started.starts_with("Started job 1"));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(shell.job_output(1).unwrap(), "[job 1 is running]\nready");
        assert_eq!(shell.job_output(1).unwrap(), "[job 1 is running; no new output]");
        assert_eq!(shell.kill_job(1).await.unwrap(), "Stopped job 1.");
        assert!(shell.job_output(9).is_err());
    }

    #[tokio::test]
    async fn declines_commands_that_need_approval_and_hides_the_key() {
        let (mut shell, _dir) = shell("confirm");
        let mut policy = ConfirmPolicy::new(&["git push"]);
        let out = shell.run("git push", None, false, &mut policy, &mut Headless).await.unwrap_err();
        assert_eq!(
            out,
            "not run: this command needs the user's approval (it matches \"git push\"). Nobody can approve it here."
        );
        assert!(std::env::var_os("HOME").is_some(), "the test needs HOME set");
        assert!(!run(&mut shell, "env").await.lines().any(|l| l.starts_with("HOME=")));
    }
}
