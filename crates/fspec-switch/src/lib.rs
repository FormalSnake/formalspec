//! `fspec switch` and `fspec forget`. Each operation is a pure function that returns a [`Plan`]
//! of commands; the CLI hands the plan to a [`Runner`], which under `--root` only logs it.

mod adopt;
mod bootc;
mod classic;
mod forget;
mod machine;
#[cfg(test)]
mod tests;

use anyhow::{Result, bail};
use fspec_core::runner::{Cmd, Runner};

pub use adopt::adopt_boot;
pub use bootc::{ResetVar, has_staged, is_bootc, reset_var, to_image};
pub use classic::{Target, switch_distro, title};
pub use forget::forget;
pub use machine::{BootMount, Bootloader, Firmware, Machine, Owner, assumed, boot_owner, detect};

/// The distros `fspec switch --distro` can bootstrap.
pub const DISTROS: [&str; 4] = ["fedora", "arch", "debian", "void"];

/// The runner could not start the program because it is not installed.
pub(crate) fn not_installed(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

pub(crate) fn subvol(distro: &str) -> String {
    format!("@{distro}")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Run(Cmd),
    /// A read-only probe whose exit status must match `ok`, else the plan stops with `msg`.
    Expect {
        probe: Cmd,
        ok: bool,
        msg: String,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub steps: Vec<Step>,
    /// Unmounts, run after the steps whether or not they succeeded.
    pub cleanup: Vec<Cmd>,
}

impl Plan {
    pub(crate) fn run(&mut self, cmd: Cmd) {
        self.steps.push(Step::Run(cmd));
    }

    pub(crate) fn expect(&mut self, probe: Cmd, ok: bool, msg: impl Into<String>) {
        self.steps.push(Step::Expect { probe, ok, msg: msg.into() });
    }

    /// Every command in execution order, probes and cleanup included.
    pub fn lines(&self) -> Vec<String> {
        let steps = self.steps.iter().map(|s| match s {
            Step::Run(c) => c.to_string(),
            Step::Expect { probe, .. } => probe.to_string(),
        });
        steps.chain(self.cleanup.iter().map(ToString::to_string)).collect()
    }

    /// Hand every command to `runner` and ignore the answers. Under `--root` the runner only logs,
    /// so probes cannot steer the plan.
    pub fn log(&self, runner: &dyn Runner) -> Result<()> {
        let all = self.steps.iter().map(|s| match s {
            Step::Run(c) => c,
            Step::Expect { probe, .. } => probe,
        });
        all.chain(&self.cleanup).try_for_each(|c| runner.run(c).map(drop))
    }

    pub fn execute(&self, runner: &dyn Runner) -> Result<()> {
        let result = self.steps.iter().try_for_each(|s| match s {
            Step::Run(c) => runner.check(c).map(drop),
            Step::Expect { probe, ok, msg } => {
                if (runner.run(probe)?.status == 0) == *ok {
                    Ok(())
                } else {
                    bail!("{msg}")
                }
            }
        });
        // A failed unmount must not hide the error that stopped the steps.
        for c in &self.cleanup {
            let _ = runner.run(c);
        }
        result
    }
}
