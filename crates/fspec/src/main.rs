mod backends;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use fspec_core::apply::{apply_scope, plan_scope, state_dir};
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
    /// Pull an app's edit of a managed file back into the repo.
    Adopt {
        /// The file as written in the spec (`~/.config/x`) or its path on disk.
        path: String,
        #[arg(long)]
        host: Option<String>,
    },
    /// Report drift on managed files as it happens.
    Watch,
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
                    sudo_system_apply(&spec_path, &host.name, &paths)?;
                    continue;
                }
                let state = State::load(&state_dir(&paths, scope).join("state.json"))?;
                let plan = apply_scope(&env.cx(scope, &state), &backends::all(), &host.desired())?;
                println!("{plan}");
            }
            Ok(())
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
/// hands the system scope to `sudo fspec apply --scope system`.
fn sudo_system_apply(spec: &Path, host: &str, paths: &Paths) -> Result<()> {
    let exe = std::env::current_exe()?;
    let status = std::process::Command::new("sudo")
        .arg(exe)
        .arg("--spec")
        .arg(spec)
        .arg("--home")
        .arg(&paths.home)
        .args(["apply", "--scope", "system", "--host", host])
        .status()
        .context("running sudo")?;
    if !status.success() {
        bail!("system scope apply failed");
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
