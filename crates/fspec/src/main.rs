mod backends;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use fspec_core::apply::{apply_scope, desired_from_generation, plan_scope, state_dir};
use fspec_core::facts::{Facts, local_hostname};
use fspec_core::fetch::LiveFetch;
use fspec_core::resource::System;
use fspec_core::runner::{Cmd, LiveRunner, LogRunner, Runner};
use fspec_core::{Cx, Host, Lock, Paths, Plan, Scope, State};
use fspec_remote::{Planned, Probe, Remote, Target};

#[derive(Parser)]
#[command(version, about = "Declare your machines in Starlark and keep them identical")]
struct Cli {
    /// The spec entry point.
    #[arg(short, long, global = true, default_value = "spec.star")]
    spec: PathBuf,
    /// Treat this directory as `/`. Everything fspec reads or writes stays under it.
    #[arg(long, global = true, default_value = "/")]
    root: PathBuf,
    /// The user's home, default $HOME (with --root, `<root>/home/<user>`).
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show what apply would change on this host.
    Plan {
        /// Host to plan, default this machine's host name.
        #[arg(long)]
        host: Option<String>,
        /// Print the plan as JSON.
        #[arg(long, conflicts_with = "all")]
        json: bool,
        /// Plan every host in the spec, asking each remote host over ssh.
        #[arg(long, conflicts_with = "host")]
        all: bool,
        /// Ship a spec tree with uncommitted changes to remote hosts anyway.
        #[arg(long)]
        dirty: bool,
    },
    /// Converge a host, over ssh when it is not this machine.
    Apply {
        #[arg(long)]
        host: Option<String>,
        /// Apply every host in the spec, remote ones first and this machine last; stops at the first failure.
        #[arg(long, conflicts_with = "host")]
        all: bool,
        /// Ship a spec tree with uncommitted changes to remote hosts anyway.
        #[arg(long)]
        dirty: bool,
        /// Apply on this machine whatever the host is called. Set on the remote end of an ssh apply.
        #[arg(long, hide = true)]
        local: bool,
        /// Apply one scope only. Without it, the system scope runs (through sudo when needed),
        /// then the user scope.
        #[arg(long)]
        scope: Option<ScopeArg>,
    },
    /// Resolve versions and hashes into fspec.lock, next to the spec.
    Update {
        /// Only re-resolve these packages, default every package.
        pkgs: Vec<String>,
        /// Pin each image host's pushed image digest instead of resolving packages.
        #[arg(long, conflicts_with = "pkgs")]
        images: bool,
        /// Image registry namespace for --images, default `ghcr.io/<owner of the git remote>`.
        #[arg(long, requires = "images")]
        registry: Option<String>,
    },
    /// Build the bootc image of a host.
    Image {
        #[command(subcommand)]
        command: ImageCommand,
    },
    /// Continuous integration for image hosts.
    Ci {
        #[command(subcommand)]
        command: CiCommand,
    },
    /// Pull an app's edit of a managed file back into the repo.
    Adopt {
        /// The file as written in the spec (`~/.config/x`) or its path on disk.
        path: String,
        #[arg(long)]
        host: Option<String>,
    },
    /// Report drift on managed files as it happens.
    Watch,
    /// Re-apply an earlier generation as a new one. Store packages come back by moving
    /// `current`, since their old store paths are still there.
    Rollback {
        /// The generation to restore, default the one before the current.
        generation: Option<u64>,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        scope: Option<ScopeArg>,
    },
    /// Print what this machine is, as `apply` compares it with the declaration.
    #[command(hide = true)]
    Facts {
        #[arg(long)]
        json: bool,
    },
    /// Print shell code that puts `.fspec.toml` pinned tools on PATH. Run it from the shell's rc file.
    ShellHook { shell: ShellArg },
    /// Print the bin directories pinned for a directory, one per line. Called by the shell hook.
    #[command(hide = true)]
    Env {
        #[arg(long)]
        dir: PathBuf,
    },
}

#[derive(Subcommand)]
enum ImageCommand {
    /// Print the Containerfile CI builds for a host.
    Containerfile {
        #[arg(long)]
        host: Option<String>,
    },
}

#[derive(Subcommand)]
enum CiCommand {
    /// Write the workflow that builds and pushes every host's image.
    Init {
        /// Image registry namespace, default `ghcr.io/<owner of the git remote>`.
        #[arg(long)]
        registry: Option<String>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ShellArg {
    Fish,
    Zsh,
    Bash,
}

#[derive(Clone, Copy, ValueEnum)]
enum ScopeArg {
    System,
    User,
}

impl From<ScopeArg> for Scope {
    fn from(s: ScopeArg) -> Scope {
        match s {
            ScopeArg::System => Scope::System,
            ScopeArg::User => Scope::User,
        }
    }
}

const SCOPES: [Scope; 2] = [Scope::System, Scope::User];

fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = paths(&cli)?;
    let spec_path = std::path::absolute(&cli.spec)?;
    let spec_dir = spec_path.parent().context("spec has no parent directory")?.to_path_buf();
    match &cli.command {
        Command::Plan { host, json, all, dirty } => {
            let spec = fspec_eval::evaluate(&spec_path)?;
            if *all {
                return plan_all(&spec, &paths, &spec_path, *dirty);
            }
            let host = select(&spec, host.as_deref(), &paths)?;
            let plan = plan_local(&paths, host, &spec_dir)?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&plan)?);
            } else {
                println!("{plan}");
            }
            Ok(())
        }
        Command::Update { pkgs, images, registry } => {
            let spec = fspec_eval::evaluate(&spec_path)?;
            let lock_path = spec_dir.join("fspec.lock");
            let old = Lock::load(&lock_path)?;
            let (fetch, runner) = (LiveFetch::new(), LiveRunner);
            let (lock, changes, errors, what) = if *images {
                let registry = registry_for(registry.as_deref(), &runner, &spec_dir)?;
                let u = fspec_image::update_images(&spec.hosts, &old, &registry, &fetch)?;
                (u.lock, u.changes, u.errors, "image")
            } else {
                let u =
                    fspec_sources::update::update(&spec.hosts, &old, pkgs, &fspec_sources::Cx::new(&fetch, &runner))?;
                (u.lock, u.changes, u.errors, "package")
            };
            if lock != old {
                lock.save(&lock_path)?;
            }
            for line in &changes {
                println!("{line}");
            }
            if !errors.is_empty() {
                bail!("{} {what}(s) failed:\n{}", errors.len(), errors.join("\n"));
            }
            Ok(())
        }
        Command::Image { command: ImageCommand::Containerfile { host } } => {
            let spec = fspec_eval::evaluate(&spec_path)?;
            let name = host.clone().unwrap_or_else(local_hostname);
            let host = spec.host(&name)?;
            let Some(System::Image(image)) = &host.system else {
                bail!("host {name} has no image(...) system layer");
            };
            print!("{}", fspec_image::containerfile(image)?);
            Ok(())
        }
        Command::Ci { command: CiCommand::Init { registry } } => {
            let spec = fspec_eval::evaluate(&spec_path)?;
            let registry = registry_for(registry.as_deref(), &LiveRunner, &spec_dir)?;
            let hosts: Vec<String> = spec
                .hosts
                .iter()
                .filter(|h| matches!(h.system, Some(System::Image(_))))
                .map(|h| h.name.clone())
                .collect();
            let spec_name = spec_path.file_name().and_then(|n| n.to_str()).context("spec has no file name")?;
            let path = spec_dir.join(fspec_image::WORKFLOW_PATH);
            std::fs::create_dir_all(path.parent().context("workflow path has no parent")?)?;
            std::fs::write(&path, fspec_image::workflow(&registry, spec_name, &hosts)?)
                .with_context(|| format!("writing {}", path.display()))?;
            println!("wrote {}", path.display());
            Ok(())
        }
        Command::Apply { host, all, dirty, local, scope } => {
            let spec = fspec_eval::evaluate(&spec_path)?;
            let this = local_hostname();
            let remote = Remote {
                runner: &LiveRunner,
                spec_dir: spec_dir.clone(),
                spec_file: file_name(&spec_path)?,
                dirty: *dirty,
            };
            let apply_one = |h: &Host| -> Result<()> {
                match remote_alias(h, &this, &paths, *local) {
                    Some(alias) => remote.apply(h, &alias, scope.map(scope_name)),
                    None => apply_local(&paths, &spec_path, &spec_dir, h, *scope),
                }
            };
            if *all {
                if !is_live(&paths) {
                    bail!("--all applies real machines; it cannot run under --root");
                }
                return fspec_remote::apply_all(&spec.hosts, &this, apply_one);
            }
            let host = select(&spec, host.as_deref(), &paths)?;
            apply_one(host)
        }
        Command::Adopt { path, host } => {
            let spec = fspec_eval::evaluate(&spec_path)?;
            let host = select(&spec, host.as_deref(), &paths)?;
            let env = Env::new(&paths, host, &spec_dir)?;
            adopt(&env, host, path)
        }
        Command::Watch => {
            let stop = std::sync::atomic::AtomicBool::new(false);
            fspec_files::watch::run(&paths, &mut |line| println!("{line}"), &stop)
        }
        Command::Rollback { generation, host, scope } => {
            let spec = fspec_eval::evaluate(&spec_path)?;
            let host = select(&spec, host.as_deref(), &paths)?;
            let mut env = Env::new(&paths, host, &spec_dir)?;
            let scopes: Vec<Scope> = scope.map(|s| vec![s.into()]).unwrap_or(SCOPES.to_vec());
            for scope in scopes {
                let dir = state_dir(&paths, scope);
                let state = State::load(&dir.join("state.json"))?;
                let n = match generation {
                    Some(n) if *n > state.generation => continue,
                    Some(n) => *n,
                    None if state.generation == 0 => continue,
                    None => state.generation - 1,
                };
                if scope == Scope::System && is_live(&paths) && !is_root(env.runner.as_ref())? {
                    sudo_system(&spec_path, &host.name, &paths, &["rollback", &n.to_string()])?;
                    continue;
                }
                let desired = desired_from_generation(&dir, n)?;
                let past = if n == 0 {
                    State::default()
                } else {
                    State::load(&dir.join("generations").join(format!("{n}.json")))?
                };
                env.lock = fspec_store::lock_from_state(&past);
                let plan = apply_scope(&env.cx(scope, &state), &backends::all(), &desired)?;
                println!("{plan}");
            }
            Ok(())
        }
        Command::Facts { json } => {
            let probe = Probe::local(local_hostname());
            if *json {
                println!("{}", serde_json::to_string(&probe)?);
            } else {
                println!("{}: {}/{}", probe.hostname, probe.platform, probe.distro);
            }
            Ok(())
        }
        Command::ShellHook { shell } => {
            let shell = match shell {
                ShellArg::Fish => fspec_store::Shell::Fish,
                ShellArg::Zsh => fspec_store::Shell::Zsh,
                ShellArg::Bash => fspec_store::Shell::Bash,
            };
            print!("{}", fspec_store::shell_hook(shell));
            Ok(())
        }
        Command::Env { dir } => {
            for bin in fspec_store::pin_dirs(&paths, dir)? {
                println!("{}", bin.display());
            }
            Ok(())
        }
    }
}

fn scope_name(s: ScopeArg) -> &'static str {
    match s {
        ScopeArg::System => "system",
        ScopeArg::User => "user",
    }
}

fn file_name(spec_path: &Path) -> Result<String> {
    Ok(spec_path.file_name().context("spec has no file name")?.to_string_lossy().into_owned())
}

/// The ssh alias when `host` is another machine. A rooted run is always a local dry run.
fn remote_alias(host: &Host, this: &str, paths: &Paths, local: bool) -> Option<String> {
    match fspec_remote::target(host, this) {
        Target::Remote { alias } if is_live(paths) && !local => Some(alias),
        _ => None,
    }
}

fn plan_local(paths: &Paths, host: &Host, spec_dir: &Path) -> Result<Plan> {
    let env = Env::new(paths, host, spec_dir)?;
    let mut plans = Vec::new();
    for scope in SCOPES {
        let state = State::load(&state_dir(paths, scope).join("state.json"))?;
        plans.push(plan_scope(&env.cx(scope, &state), &backends::all(), &host.desired(), &state)?);
    }
    Ok(merge(plans))
}

fn plan_all(spec: &fspec_eval::Spec, paths: &Paths, spec_path: &Path, dirty: bool) -> Result<()> {
    let spec_dir = spec_path.parent().context("spec has no parent directory")?;
    let this = local_hostname();
    let remote =
        Remote { runner: &LiveRunner, spec_dir: spec_dir.to_path_buf(), spec_file: file_name(spec_path)?, dirty };
    let mut failed = 0;
    for host in &spec.hosts {
        let outcome = match remote_alias(host, &this, paths, false) {
            Some(alias) => remote.plan(host, &alias).map(|p| match p {
                Planned::Plan(plan) => plan.to_string(),
                Planned::Unreachable => format!("host {}\n  unreachable, skipped", host.name),
            }),
            None => check_declared(host).and_then(|()| plan_local(paths, host, spec_dir)).map(|p| p.to_string()),
        };
        match outcome {
            Ok(text) => println!("{text}"),
            Err(e) => {
                failed += 1;
                println!("host {}\n  error: {e:#}", host.name);
            }
        }
    }
    if failed > 0 {
        bail!("{failed} host(s) could not be planned");
    }
    Ok(())
}

fn apply_local(paths: &Paths, spec_path: &Path, spec_dir: &Path, host: &Host, scope: Option<ScopeArg>) -> Result<()> {
    let env = Env::new(paths, host, spec_dir)?;
    let scopes: Vec<Scope> = scope.map(|s| vec![s.into()]).unwrap_or(SCOPES.to_vec());
    for scope in scopes {
        if scope == Scope::System && is_live(paths) && !is_root(env.runner.as_ref())? {
            sudo_system(spec_path, &host.name, paths, &["apply", "--local"])?;
            continue;
        }
        let state = State::load(&state_dir(paths, scope).join("state.json"))?;
        let plan = apply_scope(&env.cx(scope, &state), &backends::all(), &host.desired())?;
        println!("{plan}");
    }
    Ok(())
}

fn registry_for(flag: Option<&str>, runner: &dyn Runner, spec_dir: &Path) -> Result<String> {
    match flag {
        Some(r) => Ok(r.to_string()),
        None => fspec_image::default_registry(runner, spec_dir),
    }
}

fn adopt(env: &Env, host: &Host, path: &str) -> Result<()> {
    use fspec_core::Resource;
    use fspec_files::adopt::{Outcome, adopt};
    let wanted = std::path::absolute(path)?;
    let (file, scope) = host
        .desired()
        .into_iter()
        .find_map(|d| match d.resource {
            Resource::File(f) => {
                let hit = f.path == path || env.paths.resolve(&f.path).is_ok_and(|p| p == wanted);
                hit.then(|| {
                    let scope = Resource::File(f.clone()).scope();
                    (f, scope)
                })
            }
            _ => None,
        })
        .with_context(|| format!("host {} declares no file at {path}", host.name))?;
    let state_path = state_dir(env.paths, scope).join("state.json");
    let mut state = State::load(&state_path)?;
    let key = Resource::File(file.clone()).key();
    let record = state.entries.get(&key).map(|e| e.record.clone()).unwrap_or_default();
    match adopt(&env.cx(scope, &state), &file, &record)? {
        Outcome::InSync => println!("{} is what fspec wrote, nothing to adopt", file.path),
        Outcome::Copied { to, record } => {
            if let Some(entry) = state.entries.get_mut(&key) {
                entry.record = record;
                state.save(&state_path)?;
            }
            println!("copied {} into {}", file.path, to.display());
        }
        Outcome::Patch { notes } => {
            println!("{} owns keys, not the file; these changed on disk:", file.path);
            notes.iter().for_each(|n| println!("  {n}"));
        }
        Outcome::Generated { diff } => {
            print!("{diff}");
            bail!(
                "{} is generated from the spec, so there is no repo file to update; edit the spec to match",
                file.path
            );
        }
    }
    Ok(())
}

/// What every scope's `Cx` borrows.
struct Env<'a> {
    paths: &'a Paths,
    facts: Facts,
    spec_dir: &'a Path,
    lock: Lock,
    runner: Box<dyn Runner>,
    fetch: LiveFetch,
}

impl<'a> Env<'a> {
    fn new(paths: &'a Paths, host: &Host, spec_dir: &'a Path) -> Result<Env<'a>> {
        Ok(Env {
            paths,
            facts: Facts::declared(&host.name, &host.platform, host.system.as_ref())?,
            spec_dir,
            lock: Lock::load(&spec_dir.join("fspec.lock"))?,
            runner: if is_live(paths) {
                Box::new(LiveRunner)
            } else {
                Box::new(LogRunner { log: paths.system("/var/log/fspec-commands.log") })
            },
            fetch: LiveFetch::new(),
        })
    }

    fn cx(&self, scope: Scope, state: &State) -> Cx<'_> {
        Cx {
            paths: self.paths,
            facts: &self.facts,
            spec_dir: self.spec_dir,
            lock: &self.lock,
            runner: self.runner.as_ref(),
            fetch: &self.fetch,
            generation: state.generation + 1,
            scope,
        }
    }
}

fn merge(plans: Vec<Plan>) -> Plan {
    let mut all = Plan { host: plans[0].host.clone(), changes: vec![], unsupported: vec![] };
    for p in plans {
        all.changes.extend(p.changes);
        all.unsupported.extend(p.unsupported);
    }
    all.changes.sort_by(|a, b| a.key.cmp(&b.key));
    all
}

fn select<'s>(spec: &'s fspec_eval::Spec, host: Option<&str>, paths: &Paths) -> Result<&'s Host> {
    let name = host.map(str::to_owned).unwrap_or_else(local_hostname);
    let host = spec.host(&name)?;
    if is_live(paths) {
        check_declared(host)?;
    }
    Ok(host)
}

fn paths(cli: &Cli) -> Result<Paths> {
    let live = Paths::live()?;
    let home = match (&cli.home, is_live_root(&cli.root)) {
        (Some(h), _) => h.clone(),
        (None, true) => live.home,
        (None, false) => {
            let user = live.home.file_name().map(PathBuf::from).unwrap_or_else(|| "user".into());
            cli.root.join("home").join(user)
        }
    };
    Ok(Paths::new(cli.root.clone(), home))
}

fn is_live_root(root: &Path) -> bool {
    root == Path::new("/")
}

fn is_live(paths: &Paths) -> bool {
    is_live_root(&paths.root)
}

fn is_root(runner: &dyn Runner) -> Result<bool> {
    Ok(runner.check(&Cmd::new("id").arg("-u"))?.stdout.trim() == "0")
}

/// The system scope needs root and the user scope must not run as root, so a non-root apply
/// or rollback hands the system scope to `sudo fspec <command> --scope system`.
fn sudo_system(spec: &Path, host: &str, paths: &Paths, command: &[&str]) -> Result<()> {
    let exe = std::env::current_exe()?;
    let status = std::process::Command::new("sudo")
        .arg(exe)
        .arg("--spec")
        .arg(spec)
        .arg("--home")
        .arg(&paths.home)
        .args(command)
        .args(["--scope", "system", "--host", host])
        .status()
        .context("running sudo")?;
    if !status.success() {
        bail!("system scope {} failed", command[0]);
    }
    Ok(())
}

/// A host's declared platform must match the machine before fspec acts on it.
fn check_declared(host: &Host) -> Result<()> {
    let facts = Facts::declared(&host.name, &host.platform, host.system.as_ref())?;
    let (platform, distro) = Facts::probe_local();
    if facts.platform() != platform {
        bail!("host {} is declared as {} but this machine is {platform}", host.name, facts.platform());
    }
    if facts.distro != distro {
        bail!("host {} is declared as {} but this machine runs {distro}", host.name, facts.distro);
    }
    Ok(())
}
