use fspec_core::Backend;
use fspec_os::{DefaultsBackend, LaunchdBackend, SystemdBackend, UsersBackend};

/// Every backend this build knows. Each claims the resources it can apply on a given host.
pub fn all() -> Vec<&'static dyn Backend> {
    vec![&SystemdBackend, &LaunchdBackend, &UsersBackend, &DefaultsBackend]
}
