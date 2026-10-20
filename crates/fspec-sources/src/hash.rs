use anyhow::{Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest, Sha256};

use crate::Cx;

pub fn sri(bytes: &[u8]) -> String {
    format!("sha256-{}", STANDARD.encode(Sha256::digest(bytes)))
}

/// `sha256:<hex>` as the GitHub API reports asset digests, to SRI.
pub fn sri_from_digest(digest: &str) -> Result<String> {
    let Some(hex) = digest.strip_prefix("sha256:") else { bail!("unsupported digest `{digest}`") };
    if hex.len() % 2 != 0 {
        bail!("odd-length digest `{digest}`");
    }
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
        .collect::<Result<Vec<u8>, _>>()
        .map_err(|_| anyhow::anyhow!("digest `{digest}` is not hex"))?;
    Ok(format!("sha256-{}", STANDARD.encode(bytes)))
}

/// The API digest when there is one, otherwise download and hash.
pub fn hash_of(url: &str, digest: Option<&str>, cx: &Cx) -> Result<String> {
    match digest {
        Some(d) => sri_from_digest(d),
        None => Ok(sri(&cx.fetch.get(url)?)),
    }
}
