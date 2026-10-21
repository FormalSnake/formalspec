use std::path::Path;

use anyhow::Result;
use fspec_core::apply::{Backend, Cx, Op, Phase, Records};
use fspec_core::journal::Journal;
use fspec_core::resource::Resource;
use fspec_core::state::Entry;

use crate::family::Family;
use crate::pkgs::{PkgEngine, family_backend};

family_backend!(
    /// Packages from the distro's repos on a classic Fedora. The RPM the lock names is cached under
    /// `/var/cache/fspec/pkgs`, so a pin stays installable after Fedora drops the version.
    DnfBackend,
    "dnf",
    Family::Dnf
);

family_backend!(
    /// Packages from the Arch repos. The locked `.pkg.tar.zst` is cached and installed with `pacman -U`.
    PacmanBackend,
    "pacman",
    Family::Pacman
);

family_backend!(
    /// Packages from Debian or Ubuntu repos. The locked `.deb` is cached and installed by path.
    AptBackend,
    "apt",
    Family::Apt
);

family_backend!(
    /// Packages from the Void repos. The locked `.xbps` is cached in a local repository directory.
    XbpsBackend,
    "xbps",
    Family::Xbps
);

family_backend!(
    /// Packages from the AerynOS repositories. They lock by name only: moss installs from its
    /// repository stream, and a rollback activates the moss state from before the apply.
    MossBackend,
    "moss",
    Family::Moss
);
