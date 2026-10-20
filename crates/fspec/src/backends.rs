use fspec_core::Backend;
use fspec_files::FileBackend;
use fspec_store::StoreBackend;

/// Every backend this build knows. Each claims the resources it can apply on a given host.
pub fn all() -> Vec<&'static dyn Backend> {
    vec![&FileBackend, &StoreBackend]
}
