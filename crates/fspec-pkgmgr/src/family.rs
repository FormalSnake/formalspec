use std::path::Path;

use fspec_core::resource::Source;
use fspec_core::runner::Cmd;

/// The four distro package managers FormalSpec drives. Everything that differs between them is
/// a command line, so the install, cache and journal logic is shared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Family {
    Dnf,
    Pacman,
    Apt,
    Xbps,
}

impl Family {
    pub fn of_distro(distro: &str) -> Option<Family> {
        match distro {
            "fedora" => Some(Family::Dnf),
            "arch" => Some(Family::Pacman),
            "debian" | "ubuntu" => Some(Family::Apt),
            "void" => Some(Family::Xbps),
            _ => None,
        }
    }

    pub fn of_source(source: &Source) -> Option<Family> {
        match source {
            Source::Dnf(_) => Some(Family::Dnf),
            Source::Pacman(_) => Some(Family::Pacman),
            Source::Apt(_) => Some(Family::Apt),
            Source::Xbps(_) => Some(Family::Xbps),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Family::Dnf => "dnf",
            Family::Pacman => "pacman",
            Family::Apt => "apt",
            Family::Xbps => "xbps",
        }
    }

    pub fn install_names(self, names: &[String]) -> Vec<Cmd> {
        vec![match self {
            Family::Dnf => Cmd::new("dnf").args(["install", "-y"]).args(names),
            Family::Pacman => Cmd::new("pacman").args(["-S", "--noconfirm", "--needed"]).args(names),
            Family::Apt => apt("install").args(names),
            Family::Xbps => Cmd::new("xbps-install").arg("-y").args(names),
        }]
    }

    /// Install a cached package file. A local xbps repository needs its index built first, and
    /// installs by name from it.
    pub fn install_file(self, file: &Path, names: &[String]) -> Vec<Cmd> {
        let file = file.display().to_string();
        match self {
            Family::Dnf => vec![Cmd::new("dnf").args(["install", "-y"]).arg(file)],
            Family::Pacman => vec![Cmd::new("pacman").args(["-U", "--noconfirm", "--needed"]).arg(file)],
            Family::Apt => vec![apt("install").arg(file)],
            Family::Xbps => {
                let dir = Path::new(&file).parent().map(|d| d.display().to_string()).unwrap_or_default();
                vec![
                    Cmd::new("xbps-rindex").arg("-a").arg(&file),
                    Cmd::new("xbps-install").args(["-y", "-R"]).arg(dir).args(names),
                ]
            }
        }
    }

    pub fn remove(self, names: &[String]) -> Cmd {
        match self {
            Family::Dnf => Cmd::new("dnf").args(["remove", "-y"]).args(names),
            Family::Pacman => Cmd::new("pacman").args(["-R", "--noconfirm"]).args(names),
            Family::Apt => apt("remove").args(names),
            Family::Xbps => Cmd::new("xbps-remove").arg("-y").args(names),
        }
    }

    pub fn version(self, name: &str) -> Cmd {
        match self {
            Family::Dnf => Cmd::new("rpm").args(["-q", "--qf", "%{VERSION}-%{RELEASE}"]).arg(name),
            Family::Pacman => Cmd::new("pacman").arg("-Q").arg(name),
            Family::Apt => Cmd::new("dpkg-query").args(["-W", "-f=${Version}"]).arg(name),
            Family::Xbps => Cmd::new("xbps-query").args(["-p", "pkgver"]).arg(name),
        }
    }

    /// The installed version out of a successful [`Family::version`] query.
    pub fn parse_version(self, name: &str, stdout: &str) -> String {
        let out = stdout.trim();
        match self {
            Family::Pacman => out.split_whitespace().nth(1).unwrap_or_default().to_string(),
            Family::Xbps => out.strip_prefix(&format!("{name}-")).unwrap_or(out).to_string(),
            Family::Dnf | Family::Apt => out.to_string(),
        }
    }
}

fn apt(verb: &str) -> Cmd {
    Cmd::new("apt-get").args([verb, "-y"]).env("DEBIAN_FRONTEND", "noninteractive")
}
