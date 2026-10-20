use std::fmt;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// An external command. Every package manager, init system, bootc and ssh call goes through a
/// [`Runner`] so tests can record them instead of touching the machine.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
}

impl Cmd {
    pub fn new(program: impl Into<String>) -> Cmd {
        Cmd { program: program.into(), args: vec![], env: vec![], stdin: None }
    }

    pub fn arg(mut self, a: impl Into<String>) -> Cmd {
        self.args.push(a.into());
        self
    }

    pub fn args<I: IntoIterator<Item = S>, S: Into<String>>(mut self, args: I) -> Cmd {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Cmd {
        self.env.push((k.into(), v.into()));
        self
    }

    pub fn stdin(mut self, input: impl Into<String>) -> Cmd {
        self.stdin = Some(input.into());
        self
    }
}

impl fmt::Display for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.program)?;
        for a in &self.args {
            if a.contains(char::is_whitespace) { write!(f, " '{a}'")? } else { write!(f, " {a}")? }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

pub trait Runner: Sync {
    fn run(&self, cmd: &Cmd) -> Result<Output>;

    /// Run and fail on a non-zero exit, with stderr in the error.
    fn check(&self, cmd: &Cmd) -> Result<Output> {
        let out = self.run(cmd)?;
        if out.status != 0 {
            bail!("`{cmd}` exited {}: {}", out.status, out.stderr.trim());
        }
        Ok(out)
    }
}

pub struct LiveRunner;

impl Runner for LiveRunner {
    fn run(&self, cmd: &Cmd) -> Result<Output> {
        let mut c = Command::new(&cmd.program);
        c.args(&cmd.args).envs(cmd.env.iter().map(|(k, v)| (k, v)));
        c.stdin(if cmd.stdin.is_some() { Stdio::piped() } else { Stdio::null() });
        c.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = c.spawn().with_context(|| format!("running `{cmd}`"))?;
        if let Some(input) = &cmd.stdin {
            child.stdin.take().context("stdin")?.write_all(input.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        Ok(Output {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into(),
            stderr: String::from_utf8_lossy(&out.stderr).into(),
        })
    }
}

/// Appends each command to a log file instead of running it, answering `status 0`. The CLI uses
/// it whenever `--root` is not `/`, so a rooted apply never runs a package manager or init system.
pub struct LogRunner {
    pub log: std::path::PathBuf,
}

impl Runner for LogRunner {
    fn run(&self, cmd: &Cmd) -> Result<Output> {
        if let Some(dir) = self.log.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.log)?;
        writeln!(f, "{cmd}")?;
        Ok(Output::default())
    }
}

/// Records every command and answers from canned responses, `status 0` and empty output by default.
#[derive(Default)]
pub struct RecordingRunner {
    pub calls: Mutex<Vec<Cmd>>,
    /// Matched against the start of `Cmd`'s display form, first match wins.
    pub responses: Mutex<Vec<(String, Output)>>,
}

impl RecordingRunner {
    pub fn respond(&self, prefix: &str, out: Output) {
        self.responses.lock().unwrap().push((prefix.into(), out));
    }

    /// Every recorded command, display form.
    pub fn lines(&self) -> Vec<String> {
        self.calls.lock().unwrap().iter().map(ToString::to_string).collect()
    }
}

impl Runner for RecordingRunner {
    fn run(&self, cmd: &Cmd) -> Result<Output> {
        self.calls.lock().unwrap().push(cmd.clone());
        let line = cmd.to_string();
        let responses = self.responses.lock().unwrap();
        Ok(responses.iter().find(|(p, _)| line.starts_with(p.as_str())).map(|(_, o)| o.clone()).unwrap_or_default())
    }
}
