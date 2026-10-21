//! What the installer collects before it touches a disk.

use std::fmt;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Release {
    Rolling,
    Fixed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Init {
    Systemd,
    Runit,
}

/// Whether the system layer is a bootc image or a package manager driven in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    Classic,
    Image,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Preset {
    Minimal,
    Desktop,
    Gaming,
    Server,
}

impl Preset {
    /// The module file and function name in the written spec repo.
    pub fn id(self) -> &'static str {
        match self {
            Preset::Minimal => "minimal",
            Preset::Desktop => "desktop",
            Preset::Gaming => "gaming",
            Preset::Server => "server",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Desktop {
    Gnome,
    Kde,
}

impl Desktop {
    pub fn id(self) -> &'static str {
        match self {
            Desktop::Gnome => "gnome",
            Desktop::Kde => "kde",
        }
    }
}

/// A distro the installer offers. `id` is the name the written spec and `disk init --distro` use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Distro {
    pub id: &'static str,
    pub name: &'static str,
    /// One line on why someone picks it.
    pub reason: &'static str,
    pub release: Release,
    pub init: Init,
    /// The kernel carries a vendor signature that a stock Secure Boot setup trusts.
    pub signed_kernel: bool,
    pub delivery: Delivery,
    pub presets: &'static [Preset],
    /// The desktops its desktop and gaming presets come in. The first is the default.
    pub desktops: &'static [Desktop],
}

/// A value that never shows up in logs or `Debug` output.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(pub String);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Locale {
    /// `en_US.UTF-8`.
    pub locale: String,
    /// Console keymap, `us`, `de-latin1`.
    pub keymap: String,
    /// `Europe/Madrid`.
    pub timezone: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskChoice {
    /// The whole disk, `/dev/nvme0n1`.
    pub device: String,
    /// Put the btrfs filesystem in LUKS2 with this passphrase.
    pub encrypt: Option<Secret>,
    pub swap_gib: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitAuth {
    None,
    /// A private key file the user pointed at, or pasted into one under the live session's home.
    SshKey(PathBuf),
    /// An HTTPS access token.
    Token(Secret),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SystemChoice {
    /// A new spec written from a distro and a preset.
    Fresh { distro: &'static str, preset: Preset, desktop: Option<Desktop> },
    /// An existing spec repo, cloned to `checkout`, installed as host `host` declares it.
    Restore { url: String, auth: GitAuth, checkout: PathBuf, host: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    /// Login name.
    pub name: String,
    pub full_name: String,
    pub hostname: String,
    /// Set once with chpasswd in the new root; never written to the spec.
    pub password: Secret,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choices {
    pub locale: Locale,
    pub disk: DiskChoice,
    pub system: SystemChoice,
    pub account: Account,
    /// Enrol our own keys with sbctl and sign what the vendor did not.
    pub secure_boot: bool,
}
