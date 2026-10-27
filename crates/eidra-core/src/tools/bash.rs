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
use crate::vault::{Purpose, Vault};

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
    /// In an encrypted eidra directory, where the directory and environment are kept, sealed,
    /// for a session that goes on later: `state` is then in memory.
    sealed: Option<(PathBuf, Arc<Vault>)>,
    jobs: BTreeMap<u64, Job>,
    next_job: u64,
}

/// The shell's state as it is kept sealed.
#[derive(serde::Serialize, serde::Deserialize)]
struct SealedState {
    cwd: PathBuf,
    env: BTreeMap<String, String>,
}

struct Job {
    pgid: i32,
    command: String,
    started: Instant,
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
    /// must not see: the API key. `sealed` keeps the state encrypted, too, at a path of its own.
    pub fn new(
        workspace: &Path,
        state: &Path,
        secret_env: Option<&str>,
        sealed: Option<(PathBuf, Arc<Vault>)>,
    ) -> std::io::Result<Self> {
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
        let mut shell = Shell {
            cwd: workspace.to_path_buf(),
            env,
            workspace: workspace.to_path_buf(),
            state: state.to_path_buf(),
            sealed,
            jobs: BTreeMap::new(),
            next_job: 1,
        };
        // A resumed session's shell is where its last command left it.
        shell.load_state();
        shell.load_sealed();
        Ok(shell)
    }

    /// The directory and environment a resumed encrypted session's last command left.
    fn load_sealed(&mut self) {
        let Some((path, vault)) = &self.sealed else {
            return;
        };
        let Ok(sealed) = fs::read(path) else {
            return;
        };
        let state = vault.open(Purpose::File, b"shell state", &sealed).ok();
        if let Some(state) = state.and_then(|s| serde_json::from_slice::<SealedState>(&s).ok()) {
            self.cwd = state.cwd;
            self.env = state.env;
        }
    }

    /// Keeps the directory and environment sealed, for a session that goes on later.
    fn save_sealed(&self) {
        let Some((path, vault)) = &self.sealed else {
            return;
        };
        let state = SealedState { cwd: self.cwd.clone(), env: self.env.clone() };
        if let Ok(json) = serde_json::to_vec(&state) {
            let _ = fs::write(path, vault.seal(Purpose::File, b"shell state", &json));
        }
    }

    /// The directory the next command starts in.
    pub fn cwd(&self) -> &Path {
        &self.cwd
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
        let asked = timeout_s.unwrap_or(DEFAULT_TIMEOUT_S);
        let timeout = Duration::from_secs(asked.clamp(1, MAX_TIMEOUT_S));
        let mut output = self.run_foreground(command, timeout, timeout_s.is_none()).await?;
        // A longer timeout than allowed is not refused, but the model should know it was cut.
        if asked > MAX_TIMEOUT_S {
            output.push_str(&format!(
                "\n[timeout_s is at most {MAX_TIMEOUT_S}, so it ran with {MAX_TIMEOUT_S}; run \
                 longer commands with background set]"
            ));
        }
        Ok(output)
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

    async fn run_foreground(
        &mut self,
        command: &str,
        timeout: Duration,
        default: bool,
    ) -> Result<String, String> {
        let _ = fs::remove_file(self.state.join("cwd"));
        let _ = fs::remove_file(self.state.join("env"));
        let mut child = self
            .command(command, &self.state)
            .spawn()
            .map_err(|e| format!("could not start bash: {e}"))?;
        let pgid = child.id().map(|pid| pid as i32);
        // If this future is dropped (the user interrupted the agent), the group goes too.
        let mut guard = GroupGuard(pgid);
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let mut capture = Capture::default();

        let status =
            match tokio::time::timeout(timeout, collect(&mut child, &mut stdout, &mut capture))
                .await
            {
                Ok(Ok(status)) => {
                    self.load_state();
                    self.save_sealed();
                    describe_exit(status)
                }
                Ok(Err(e)) => return Err(format!("reading the command's output failed: {e}")),
                Err(_) => {
                    if let Some(pgid) = guard.0.take() {
                        stop_group(pgid).await;
                    }
                    let _ = child.wait().await;
                    let default = if default {
                        format!(
                            ", the default (timeout_s allows up to {MAX_TIMEOUT_S}, and background \
                             has no limit)"
                        )
                    } else {
                        String::new()
                    };
                    Some(format!(
                        "timed out after {} s{default}; the command and everything it started \
                         were stopped",
                        timeout.as_secs()
                    ))
                }
            };
        guard.0 = None;
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
        let (command, started) = (command.to_owned(), Instant::now());
        self.jobs.insert(id, Job { pgid, command, started, log, shown: 0 });
        Ok(format!(
            "Started job {id} in the background. Read its output with bash_output (id {id}), and stop it with bash_kill."
        ))
    }

    /// What a background job printed since it was last asked, after waiting up to `wait` for
    /// something new: more output, or its end.
    pub async fn job_output(&mut self, id: u64, wait: Duration) -> Result<String, String> {
        let job = self.jobs.get_mut(&id).ok_or_else(|| format!("there is no job {id}"))?;
        let until = Instant::now() + wait.min(Duration::from_secs(MAX_TIMEOUT_S));
        loop {
            let news = {
                let log = job.log.lock().unwrap();
                log.exit.is_some() || log.dropped + log.bytes.len() > job.shown
            };
            if news || Instant::now() >= until {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
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
        // What the job is and how long it has run, so a wait is judged against the command's own.
        let what = format!("job {id} (`{}`)", crate::display::clip(&job.command, 60));
        let took = crate::display::duration(job.started.elapsed().as_secs_f64());
        let status = match &log.exit {
            Some(exit) => format!("{what} has ended ({exit})"),
            None => format!("{what} has been running for {took}"),
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

/// Runs a command the user typed, `!` in the TUI, in a bash of its own in `dir`: nothing carries
/// over to the agent's shell or to the next command. Returns its output, cleaned of terminal
/// codes, and how it ended unless with 0. Past `timeout`, or if the future is dropped, the
/// command and everything it started are stopped. `secret_env` names a variable it must not see,
/// as the agent's commands do not: its output goes to the model.
pub async fn run_command(
    dir: &Path,
    command: &str,
    timeout: Duration,
    secret_env: Option<&str>,
) -> (String, Option<String>) {
    let mut cmd = std::process::Command::new("bash");
    if let Some(name) = secret_env {
        cmd.env_remove(name);
    }
    cmd.arg("-c")
        .arg(format!("exec 2>&1\n{command}"))
        .current_dir(dir)
        .envs(NON_INTERACTIVE.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let mut cmd = Command::from(cmd);
    cmd.kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return (String::new(), Some(format!("could not start bash: {e}"))),
    };
    let mut guard = GroupGuard(child.id().map(|pid| pid as i32));
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut capture = Capture::default();
    let status =
        match tokio::time::timeout(timeout, collect(&mut child, &mut stdout, &mut capture)).await {
            Ok(Ok(status)) => describe_exit(status),
            Ok(Err(e)) => Some(format!("reading its output failed: {e}")),
            Err(_) => {
                if let Some(pgid) = guard.0.take() {
                    stop_group(pgid).await;
                }
                let _ = child.wait().await;
                Some(format!("timed out after {} s, and was stopped", timeout.as_secs()))
            }
        };
    guard.0 = None;
    let output = clean_terminal_output(&capture.into_string());
    (output.trim_end().to_owned(), status)
}

/// SIGTERM to the whole group, then SIGKILL if it is still there after a moment.
/// Stops a foreground command's process group if the command is abandoned before it ends:
/// `kill_on_drop` stops only bash, not what the command started.
struct GroupGuard(Option<i32>);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        let Some(pgid) = self.0 else {
            return;
        };
        // SAFETY: killpg only sends signals, to a process group this shell created.
        unsafe { libc::killpg(pgid, libc::SIGTERM) };
        // What ignores SIGTERM is killed a little later, if a runtime is still there to do it.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                tokio::time::sleep(Duration::from_millis(1500)).await;
                // SAFETY: as above; signal 0 only checks that the group still exists.
                if unsafe { libc::killpg(pgid, 0) } == 0 {
                    // SAFETY: as above.
                    unsafe { libc::killpg(pgid, libc::SIGKILL) };
                }
            });
        }
    }
}

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
        (Shell::new(&dir, &dir.join(".state"), Some("HOME"), None).unwrap(), dir)
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
        // A resumed session's shell starts where the last command left it.
        let mut again = Shell::new(&_dir, &_dir.join(".state"), Some("HOME"), None).unwrap();
        assert!(run(&mut again, "pwd").await.ends_with("/sub"));
        assert_eq!(run(&mut again, "echo $GREETING").await, "hi");
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
        let none = Duration::ZERO;
        // Waiting returns as soon as there is output, long before the wait is up.
        let asked = std::time::Instant::now();
        let first = shell.job_output(1, Duration::from_secs(20)).await.unwrap();
        assert!(asked.elapsed() < Duration::from_secs(5));
        assert_eq!(first, "[job 1 (`echo ready; sleep 30`) has been running for 0 s]\nready");
        let next = shell.job_output(1, none).await.unwrap();
        assert!(next.ends_with("has been running for 0 s; no new output]"), "{next}");
        assert_eq!(shell.kill_job(1).await.unwrap(), "Stopped job 1.");
        // A job that ends is news too.
        shell.run("sleep 1; exit 3", None, true, &mut policy, &mut Headless).await.unwrap();
        let ended = shell.job_output(2, Duration::from_secs(20)).await.unwrap();
        assert_eq!(ended, "[job 2 (`sleep 1; exit 3`) has ended (exit code 3); no new output]");
        assert!(shell.job_output(9, none).await.is_err());
    }

    #[tokio::test]
    async fn says_when_a_timeout_was_cut() {
        let (mut shell, _dir) = shell("timeouts");
        let mut policy = ConfirmPolicy::new::<&str>(&[]);
        let out = shell.run("echo hi", Some(5_000), false, &mut policy, &mut Headless).await;
        assert_eq!(
            out.unwrap(),
            "hi\n[timeout_s is at most 600, so it ran with 600; run longer commands with \
             background set]"
        );
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
