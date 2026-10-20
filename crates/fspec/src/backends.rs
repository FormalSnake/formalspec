use fspec_core::Backend;
use fspec_pkgmgr::{BrewBackend, DnfBackend, FlatpakBackend};

/// Every backend this build knows. Each claims the resources it can apply on a given host.
pub fn all() -> Vec<&'static dyn Backend> {
    vec![&FlatpakBackend, &BrewBackend, &DnfBackend]
}
