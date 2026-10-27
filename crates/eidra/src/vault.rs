//! The password of an encrypted eidra directory (docs/DESIGN.md §18), asked for on the terminal
//! before anything else starts, without echo. The TUI encrypts a directory that is not yet, with
//! a password the user chooses; `eidra exec` never does, so that evals and scripts run as before,
//! but asks for the password of one that is.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use eidra_core::vault::Vault;
use zeroize::Zeroizing;

/// Set to `off`, eidra leaves its directory unencrypted and asks for no password.
const OFF: &str = "EIDRA_VAULT";
/// Tries at the password before eidra gives up.
const TRIES: usize = 3;
const SHORTEST: usize = 8;

/// The vault of eidra's directory `home`, unlocked. None if the directory is not encrypted and
/// `create` is false, or with EIDRA_VAULT=off. With `create`, a directory not encrypted yet is,
/// with a password the user chooses.
pub fn open(home: &Path, create: bool) -> Result<Option<Arc<Vault>>> {
    if std::env::var(OFF).is_ok_and(|value| value == "off") {
        return Ok(None);
    }
    let shown = shown(home);
    if Vault::exists(home) {
        let mut tty = Tty::open(&shown)?;
        for _ in 0..TRIES {
            let password = tty.ask(&format!("password for {shown}: "))?;
            if let Some(vault) = Vault::unlock(home, &password)? {
                take_in_instructions(&vault, home, &shown, &mut tty)?;
                return Ok(Some(Arc::new(vault)));
            }
            tty.say("that is not its password")?;
        }
        bail!("{TRIES} wrong passwords for {shown}");
    }
    if !create {
        return Ok(None);
    }
    let mut tty = Tty::open(&shown)?;
    // Short lines: the terminal never has to wrap them.
    tty.say(&format!(
        "{shown} is not encrypted yet.\n\
         Choose a password for it. It encrypts your sessions, your AGENTS.md and\n\
         the snapshots /undo uses, and without it none of them can be read again.\n\
         EIDRA_VAULT=off leaves it unencrypted."
    ))?;
    loop {
        let password = tty.ask("new password: ")?;
        if password.chars().count() < SHORTEST {
            tty.say(&format!("at least {SHORTEST} characters, please"))?;
            continue;
        }
        if *tty.ask("the same again: ")? != *password {
            tty.say("the two differ; once more")?;
            continue;
        }
        let vault = Vault::create(home, &password)?;
        take_in_instructions(&vault, home, &shown, &mut tty)?;
        tty.say(&format!("{shown} is encrypted"))?;
        return Ok(Some(Arc::new(vault)));
    }
}

/// Encrypts a plain AGENTS.md the user left in the directory, and says so.
fn take_in_instructions(vault: &Vault, home: &Path, shown: &str, tty: &mut Tty) -> Result<()> {
    match vault.take_in_instructions(home) {
        Ok(true) => tty.say(&format!("{shown}/AGENTS.md is encrypted now; the plain one is gone")),
        Ok(false) => Ok(()),
        Err(e) => tty.say(&format!("{shown}/AGENTS.md was left as it is: {e:#}")),
    }
}

/// `home` as the user knows it.
fn shown(home: &Path) -> String {
    let user_home = std::env::var_os("HOME").filter(|h| h.len() > 1);
    match user_home.and_then(|h| home.strip_prefix(h).ok().map(Path::to_path_buf)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => home.display().to_string(),
    }
}

/// The terminal, which the password is read from even when stdin is not it.
struct Tty(File);

impl Tty {
    fn open(shown: &str) -> Result<Self> {
        let file =
            OpenOptions::new().read(true).write(true).open("/dev/tty").with_context(|| {
                format!(
                    "{shown} is encrypted, and eidra needs a terminal to ask for its password \
                 (EIDRA_VAULT=off runs without it, and without what it encrypts)"
                )
            })?;
        Ok(Tty(file))
    }

    fn say(&mut self, text: &str) -> Result<()> {
        writeln!(self.0, "{text}").context("writing to the terminal")
    }

    /// A line typed after `prompt`, not shown as it is typed. eidra reads the keys itself, with
    /// the terminal's signals off, so that Ctrl+C ends the prompt with the terminal as it was
    /// rather than ending eidra with echo off.
    fn ask(&mut self, prompt: &str) -> Result<Zeroizing<String>> {
        write!(self.0, "{prompt}").context("writing to the terminal")?;
        self.0.flush()?;
        let mut bytes = Zeroizing::new(Vec::new());
        {
            let _quiet = Quiet::new(&self.0)?;
            let mut byte = [0u8; 1];
            loop {
                if self.0.read(&mut byte).context("reading the terminal")? == 0 {
                    bail!("the terminal closed before a password was typed");
                }
                match byte[0] {
                    b'\r' | b'\n' => break,
                    // Ctrl+C, or Ctrl+D with nothing typed: no password.
                    3 => {
                        writeln!(self.0)?;
                        bail!("no password given");
                    }
                    4 if bytes.is_empty() => {
                        writeln!(self.0)?;
                        bail!("no password given");
                    }
                    // Ctrl+D after something typed, as a terminal's line editing treats it.
                    4 => {}
                    // Backspace: the last character, all its bytes.
                    8 | 127 => {
                        while let Some(b) = bytes.pop() {
                            if b & 0xc0 != 0x80 {
                                break;
                            }
                        }
                    }
                    // Ctrl+U: all of it.
                    21 => bytes.clear(),
                    b => bytes.push(b),
                }
            }
        }
        writeln!(self.0).context("writing to the terminal")?;
        Ok(Zeroizing::new(String::from_utf8_lossy(&bytes).into_owned()))
    }
}

/// The terminal without echo, line editing or signals until dropped: keys come one at a time.
struct Quiet {
    fd: i32,
    before: libc::termios,
}

impl Quiet {
    fn new(file: &File) -> Result<Self> {
        let fd = file.as_raw_fd();
        // SAFETY: a zeroed termios is a valid value for tcgetattr to fill in, and `fd` is open.
        let mut before: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut before) } != 0 {
            bail!("reading the terminal's settings: {}", std::io::Error::last_os_error());
        }
        let mut quiet = before;
        quiet.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG);
        quiet.c_cc[libc::VMIN] = 1;
        quiet.c_cc[libc::VTIME] = 0;
        // SAFETY: `quiet` came from tcgetattr on the same descriptor.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) } != 0 {
            bail!("setting the terminal up for a password: {}", std::io::Error::last_os_error());
        }
        Ok(Quiet { fd, before })
    }
}

impl Drop for Quiet {
    fn drop(&mut self) {
        // SAFETY: as in new; the settings are the ones the terminal had.
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.before) };
    }
}
