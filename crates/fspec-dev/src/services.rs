//! A project's services: started when its first `fspec dev` shell opens and stopped when the last
//! one exits, unless `fspec dev up` started them, which keeps them until `fspec dev down`.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use fspec_core::Lock;
use fspec_core::dev::DevService;
use fspec_core::runner::{Cmd, Runner};

use crate::{Project, cache_dir};

/// Where one project's service state lives.
pub struct State {
    dir: PathBuf,
    id: String,
}

impl State {
    pub fn new(home: &Path, project: &Project) -> State {
        State { dir: cache_dir(home).join("run").join(project.id()), id: project.id() }
    }

    fn container(&self, name: &str) -> String {
        format!("fspec-{}-{name}", self.id)
    }

    /// Note a shell as open.
    pub fn enter(&self) -> Result<()> {
        fs::create_dir_all(self.dir.join("sessions"))?;
        fs::write(self.dir.join("sessions").join(std::process::id().to_string()), "")?;
        Ok(())
    }

    /// Note this shell as closed; true when it was the last one and nothing asked for the
    /// services to stay up.
    pub fn leave(&self) -> Result<bool> {
        let sessions = self.dir.join("sessions");
        let _ = fs::remove_file(sessions.join(std::process::id().to_string()));
        let mut open = 0;
        for e in fs::read_dir(&sessions).into_iter().flatten().flatten() {
            match e.file_name().to_string_lossy().parse::<i32>() {
                Ok(pid) if alive(pid) => open += 1,
                _ => {
                    let _ = fs::remove_file(e.path());
                }
            }
        }
        Ok(open == 0 && !self.dir.join("up").exists())
    }

    pub fn mark_up(&self, up: bool) -> Result<()> {
        fs::create_dir_all(&self.dir)?;
        let marker = self.dir.join("up");
        if up {
            fs::write(marker, "")?;
        } else if marker.exists() {
            fs::remove_file(marker)?;
        }
        Ok(())
    }

    /// Start every service that is not running. Processes get the shell's environment;
    /// images run detached with their exposed ports on 127.0.0.1 and named volumes.
    pub fn start(
        &self,
        services: &[DevService],
        lock: &Lock,
        env: &BTreeMap<String, String>,
        cwd: &Path,
        runner: &dyn Runner,
        runtime: Option<&str>,
    ) -> Result<()> {
        fs::create_dir_all(self.dir.join("logs"))?;
        for s in services {
            match &s.exec {
                Some(exec) => self.start_process(s, exec, env, cwd)?,
                None => {
                    let runtime = runtime
                        .with_context(|| format!("service {} runs an image, which needs docker or podman", s.name))?;
                    self.start_image(s, lock, runner, runtime)?;
                }
            }
        }
        Ok(())
    }

    fn start_process(&self, s: &DevService, exec: &str, env: &BTreeMap<String, String>, cwd: &Path) -> Result<()> {
        let pidfile = self.dir.join(format!("{}.pid", s.name));
        if read_pid(&pidfile).is_some_and(alive) {
            return Ok(());
        }
        let log = fs::File::create(self.dir.join("logs").join(format!("{}.log", s.name)))?;
        let child = Command::new("sh")
            .args(["-c", exec])
            .env_clear()
            .envs(env)
            .envs(&s.env)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .process_group(0)
            .spawn()
            .with_context(|| format!("starting service {}", s.name))?;
        fs::write(&pidfile, child.id().to_string())?;
        eprintln!("fspec: started {} (pid {}, log {})", s.name, child.id(), self.dir.join("logs").display());
        Ok(())
    }

    fn start_image(&self, s: &DevService, lock: &Lock, runner: &dyn Runner, runtime: &str) -> Result<()> {
        let reference = s.image_ref().context("a service without exec runs an image")?;
        let pinned = lock.image.get(&reference).with_context(|| {
            format!("service {}: {reference} is not pinned in fspec.lock, run fspec update", s.name)
        })?;
        let name = self.container(&s.name);
        let running = runner.run(&Cmd::new(runtime).args(["inspect", "-f", "{{.State.Running}}", &name]))?;
        if running.stdout.trim() == "true" {
            return Ok(());
        }
        let have = runner.run(&Cmd::new(runtime).args(["image", "inspect", pinned]))?;
        if have.status != 0 {
            runner.check(&Cmd::new(runtime).args(["pull", "-q", pinned]))?;
        }
        let meta = runner.check(&Cmd::new(runtime).args([
            "image",
            "inspect",
            "-f",
            "{{json .Config.ExposedPorts}} {{json .Config.Volumes}}",
            pinned,
        ]))?;
        let (ports, volumes) = exposed(&meta.stdout);
        let ports = if s.ports.is_empty() { ports } else { s.ports.clone() };
        let mut cmd = Cmd::new(runtime).args(["run", "-d", "--rm", "--name", &name]);
        for p in &ports {
            cmd = cmd.args(["-p".to_string(), format!("127.0.0.1:{p}:{p}")]);
        }
        for (i, v) in volumes.iter().enumerate() {
            cmd = cmd.args(["-v".to_string(), format!("{name}-{i}:{v}")]);
        }
        for (k, v) in &s.env {
            cmd = cmd.args(["-e".to_string(), format!("{k}={v}")]);
        }
        runner.check(&cmd.arg(pinned.as_str())).with_context(|| format!("starting service {}", s.name))?;
        fs::write(self.dir.join(format!("{}.container", s.name)), &name)?;
        let shown: Vec<String> = ports.iter().map(|p| format!("127.0.0.1:{p}")).collect();
        eprintln!("fspec: started {} ({reference}) on {}", s.name, shown.join(", "));
        Ok(())
    }

    /// Stop everything this project started.
    pub fn stop(&self, runner: &dyn Runner, runtime: Option<&str>) -> Result<()> {
        for e in fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let path = e.path();
            let file = e.file_name().to_string_lossy().into_owned();
            if let Some(name) = file.strip_suffix(".pid") {
                if let Some(pid) = read_pid(&path) {
                    stop_group(pid);
                    eprintln!("fspec: stopped {name}");
                }
                fs::remove_file(&path)?;
            } else if let Some(name) = file.strip_suffix(".container") {
                let container = fs::read_to_string(&path)?;
                let runtime = runtime.context("stopping a service container needs docker or podman")?;
                runner.check(&Cmd::new(runtime).args(["stop", container.trim()]))?;
                eprintln!("fspec: stopped {name}");
                fs::remove_file(&path)?;
            }
        }
        Ok(())
    }
}

/// `{"5432/tcp":{}} {"/var/lib/postgresql/data":{}}` as ports and volume paths.
fn exposed(inspect: &str) -> (Vec<u16>, Vec<String>) {
    let mut parts = inspect.trim().splitn(2, ' ');
    let parse = |s: Option<&str>| -> Vec<String> {
        match s.and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()) {
            Some(serde_json::Value::Object(m)) => m.keys().cloned().collect(),
            _ => vec![],
        }
    };
    let ports = parse(parts.next()).iter().filter_map(|p| p.split('/').next()?.parse().ok()).collect();
    (ports, parse(parts.next()))
}

fn read_pid(path: &Path) -> Option<i32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// TERM the service's process group, then KILL it after ten seconds.
fn stop_group(pid: i32) {
    // SAFETY: the pid was started by `start_process` as its own process group leader.
    unsafe { libc::kill(-pid, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(10);
    // A service this process started stays a zombie, and so alive, until it is waited for.
    // SAFETY: WNOHANG never blocks, and waiting on a pid that is not our child only fails.
    let reaped = || unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) == pid };
    while !reaped() && alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    if alive(pid) {
        // SAFETY: as above.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
}

/// Fails when a service name could escape the state directory or the container name.
pub fn check_names(services: &[DevService]) -> Result<()> {
    for s in services {
        if s.name.is_empty() || !s.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            bail!("service name `{}` must be letters, digits, - and _", s.name);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposed_ports_and_volumes_come_from_the_image_config() {
        let (ports, vols) = exposed("{\"5432/tcp\":{}} {\"/var/lib/postgresql/data\":{}}\n");
        assert_eq!(ports, [5432]);
        assert_eq!(vols, ["/var/lib/postgresql/data"]);
        assert_eq!(exposed("null null"), (vec![], vec![]));
    }

    #[test]
    fn process_services_start_once_and_stop_with_the_last_shell() {
        let home = tempfile::tempdir().unwrap();
        let project = Project { dir: home.path().join("proj") };
        fs::create_dir_all(&project.dir).unwrap();
        let state = State::new(home.path(), &project);
        let svc = DevService {
            name: "sleeper".into(),
            exec: Some("sleep 30".into()),
            version: None,
            image: None,
            ports: vec![],
            env: BTreeMap::new(),
        };
        let runner = fspec_core::runner::RecordingRunner::default();
        let env: BTreeMap<String, String> = [("PATH".to_string(), "/usr/bin:/bin".to_string())].into();
        state.enter().unwrap();
        state.start(std::slice::from_ref(&svc), &Lock::default(), &env, &project.dir, &runner, None).unwrap();
        let pid = read_pid(&state.dir.join("sleeper.pid")).unwrap();
        assert!(alive(pid));
        state.start(std::slice::from_ref(&svc), &Lock::default(), &env, &project.dir, &runner, None).unwrap();
        assert_eq!(read_pid(&state.dir.join("sleeper.pid")), Some(pid));
        assert!(state.leave().unwrap());
        let started = Instant::now();
        state.stop(&runner, None).unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!alive(pid));
    }
}
