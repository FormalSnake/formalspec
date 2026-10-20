mod backends;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use fspec_core::apply::{apply_scope, desired_from_generation, plan_scope, state_dir};
use fspec_core::facts::{Facts, local_hostname};
use fspec_core::fetch::LiveFetch;
use fspec_core::runner::{Cmd, LiveRunner, LogRunner, Runner};
use fspec_core::{Cx, Host, Lock, Paths, Plan, Scope, State};

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
        #[arg(long)]
        json: bool,
    },
    /// Converge this host.
    Apply {
        #[arg(long)]
        host: Option<String>,
        /// Apply one scope only. Without it, the system scope runs (through sudo when needed),
        /// then the user scope.
        #[arg(long)]
        scope: Option<ScopeArg>,
    },
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
    /// Print shell code that puts `.fspec.toml` pinned tools on PATH. Run it from the shell's rc file.
    ShellHook { shell: ShellArg },
    /// Print the bin directories pinned for a directory, one per line. Called by the shell hook.
    #[command(hide = true)]
    Env {
        #[arg(long)]
        dir: PathBuf,
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
        Command::Plan { host, json } => {
            let spec = fspec_eval::evaluate(&spec_path)?;
            let host = select(&spec, host.as_deref(), &paths)?;
            let env = Env::new(&paths, host, &spec_dir)?;
            let mut plans = Vec::new();
            for scope in SCOPES {
                let state = State::load(&state_dir(&paths, scope).join("state.json"))?;
                plans.push(plan_scope(&env.cx(scope, &state), &backends::all(), &host.desired(), &state)?);
            }
            let plan = merge(plans);
            if *json {
                println!("{}", serde_json::to_string_pretty(&plan)?);
            } else {
                println!("{plan}");
            }
            Ok(())
        }
        Command::Apply { host, scope } => {
            let spec = fspec_eval::evaluate(&spec_path)?;
            let host = select(&spec, host.as_deref(), &paths)?;
            let env = Env::new(&paths, host, &spec_dir)?;
            let scopes: Vec<Scope> = scope.map(|s| vec![s.into()]).unwrap_or(SCOPES.to_vec());
            for scope in scopes {
                if scope == Scope::System && is_live(&paths) && !is_root(env.runner.as_ref())? {
                    sudo_system(&spec_path, &host.name, &paths, &["apply"])?;
                    continue;
                }
                let state = State::load(&state_dir(&paths, scope).join("state.json"))?;
                let plan = apply_scope(&env.cx(scope, &state), &backends::all(), &host.desired())?;
                println!("{plan}");
            }
            Ok(())
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
