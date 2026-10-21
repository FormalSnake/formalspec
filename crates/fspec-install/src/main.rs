use std::path::PathBuf;
use std::process::{Command, exit};
use std::sync::Arc;

use anyhow::{Result, bail};
use clap::Parser;
use fspec_install::backend::Backend;
use fspec_install::demo::DemoBackend;
use fspec_install::live::Live;
use fspec_install::ui::{self, Exit};

#[derive(Parser)]
#[command(version, about = "Install a Linux system that FormalSpec manages from its first boot")]
struct Args {
    /// Run against a made-up machine; nothing is written anywhere.
    #[arg(long)]
    demo: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let (backend, checkout): (Arc<dyn Backend>, PathBuf) = if args.demo {
        (Arc::new(DemoBackend::new()), std::env::temp_dir().join("fspec-install-demo"))
    } else {
        if !is_root() {
            bail!("the installer partitions disks, so it runs as root");
        }
        (Arc::new(Live::new()), PathBuf::from("/run/fspec-install/clone"))
    };
    match ui::run(backend, checkout)? {
        Exit::Restart => {
            Command::new("systemctl").arg("reboot").status()?;
        }
        Exit::Shell => exit(2),
        Exit::Quit => {}
    }
    Ok(())
}

fn is_root() -> bool {
    Command::new("id").arg("-u").output().is_ok_and(|o| o.stdout.trim_ascii() == b"0")
}
