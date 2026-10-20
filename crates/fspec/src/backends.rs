use fspec_core::Backend;
use fspec_files::FileBackend;
use fspec_image::ImageBackend;
use fspec_os::{
    DefaultsBackend, DinitBackend, LaunchdBackend, OpenrcBackend, RunitBackend, S6Backend, SystemdBackend, UsersBackend,
};
use fspec_pkgmgr::{AptBackend, BrewBackend, DistroBackend, DnfBackend, FlatpakBackend, PacmanBackend, XbpsBackend};
use fspec_store::StoreBackend;

/// Every backend this build knows. Each claims the resources it can apply on a given host.
pub fn all() -> Vec<&'static dyn Backend> {
    vec![
        &UsersBackend,
        &FileBackend,
        &StoreBackend,
        &FlatpakBackend,
        &BrewBackend,
        &DnfBackend,
        &PacmanBackend,
        &AptBackend,
        &XbpsBackend,
        &DistroBackend,
        &SystemdBackend,
        &RunitBackend,
        &OpenrcBackend,
        &DinitBackend,
        &S6Backend,
        &LaunchdBackend,
        &DefaultsBackend,
        &ImageBackend,
    ]
}
