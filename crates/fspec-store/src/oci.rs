//! Packages built by CI are pulled from an OCI registry with the `oras` CLI, through the
//! runner, so the host needs `oras` on PATH and no compiler.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use fspec_core::runner::{Cmd, Runner};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
struct Manifest {
    layers: Vec<Layer>,
}

#[derive(Deserialize)]
struct Layer {
    digest: String,
    #[serde(default)]
    annotations: std::collections::BTreeMap<String, String>,
}

/// Pull `reference` (`registry/repo@sha256:...`) into `dir` and return the artifact's bytes.
/// `oras` checks the manifest against the digest in the reference; the layer file is then
/// hashed again against the layer digest the manifest names.
pub fn pull(runner: &dyn Runner, reference: &str, dir: &Path) -> Result<Vec<u8>> {
    if !reference.contains("@sha256:") {
        bail!("`{reference}` is not pinned to a digest, run fspec update --cache");
    }
    fs::create_dir_all(dir)?;
    runner.check(&Cmd::new("oras").args(["pull", reference, "-o"]).arg(dir.display().to_string()))?;
    let manifest = runner.check(&Cmd::new("oras").args(["manifest", "fetch", reference]))?;
    let manifest: Manifest = serde_json::from_str(&manifest.stdout).context("parsing the OCI manifest")?;
    let [layer] = manifest.layers.as_slice() else { bail!("{reference} should hold exactly one layer") };
    let title = layer.annotations.get("org.opencontainers.image.title").context("the layer has no file name")?;
    if title.contains('/') || title.starts_with('.') {
        bail!("the layer is named `{title}`, which is not a plain file name");
    }
    let body = fs::read(dir.join(title)).with_context(|| format!("oras pulled no {title}"))?;
    let want = layer.digest.strip_prefix("sha256:").context("the layer digest is not sha256")?;
    let got = sha256_hex(&body);
    if got != want {
        bail!("{title} hashes to sha256:{got} but the manifest says {}", layer.digest);
    }
    Ok(body)
}

fn sha256_hex(body: &[u8]) -> String {
    Sha256::digest(body).iter().map(|b| format!("{b:02x}")).collect()
}
