//! The bootc half of the system layer: the Containerfile and CI workflow generated from
//! `image(...)`, digest resolution for `fspec update --images`, and the backend that runs
//! `bootc switch`.

mod backend;
mod ci;
mod containerfile;
mod registry;
#[cfg(test)]
mod testutil;

pub use backend::ImageBackend;
pub use ci::{WORKFLOW_PATH, default_registry, workflow};
pub use containerfile::containerfile;
pub use registry::{ImageUpdate, pin_image, repo, resolve_digest, update_images};

/// Tag CI pushes and `fspec update --images` resolves.
pub const TAG: &str = "latest";
