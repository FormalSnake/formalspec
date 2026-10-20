use std::fs;
use std::io::Read;
use std::path::Path;

use age::armor::ArmoredReader;
use anyhow::{Context, Result, bail};

/// Decrypt an age file (binary or armored) with the identities in `identity_file`.
pub fn decrypt(src: &Path, identity_file: &Path) -> Result<Vec<u8>> {
    let ciphertext = fs::read(src).with_context(|| format!("reading {}", src.display()))?;
    let key = fs::File::open(identity_file)
        .with_context(|| format!("opening the age identity {}", identity_file.display()))?;
    let identities = age::IdentityFile::from_buffer(std::io::BufReader::new(key))
        .with_context(|| format!("reading {}", identity_file.display()))?
        .into_identities()
        .with_context(|| format!("loading {}", identity_file.display()))?;
    if identities.is_empty() {
        bail!("{} holds no identities", identity_file.display());
    }
    let decryptor = age::Decryptor::new(ArmoredReader::new(&ciphertext[..]))
        .with_context(|| format!("{} is not an age file", src.display()))?;
    let mut reader = decryptor
        .decrypt(identities.iter().map(|i| i.as_ref() as &dyn age::Identity))
        .with_context(|| format!("decrypting {}", src.display()))?;
    let mut plain = Vec::new();
    reader.read_to_end(&mut plain)?;
    Ok(plain)
}
