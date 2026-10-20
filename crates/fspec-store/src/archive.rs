use std::fs;
use std::io::{Cursor, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest, Sha256, Sha512};

/// `sha256-<base64>` of `body`, the form `fspec.lock` stores.
pub fn sri_sha256(body: &[u8]) -> String {
    format!("sha256-{}", STANDARD.encode(Sha256::digest(body)))
}

/// Check `body` against an SRI string (`sha256-…` or `sha512-…`).
pub fn verify(body: &[u8], sri: &str) -> Result<()> {
    let (algo, want) = sri.split_once('-').with_context(|| format!("hash `{sri}` is not <algo>-<base64>"))?;
    let got = match algo {
        "sha256" => STANDARD.encode(Sha256::digest(body)),
        "sha512" => STANDARD.encode(Sha512::digest(body)),
        other => bail!("hash algorithm `{other}` is not supported, use sha256 or sha512"),
    };
    if got.trim_end_matches('=') != want.trim_end_matches('=') {
        bail!("hash mismatch: the lock has {sri}, the download is {algo}-{got}");
    }
    Ok(())
}

/// Unpack a downloaded artifact into `dest`, picking the format from its magic bytes. Anything
/// that is not an archive is a single executable and lands as `dest/<name>`.
pub fn unpack(body: &[u8], dest: &Path, name: &str) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    if body.starts_with(&[0x1f, 0x8b]) {
        untar(flate2::read::GzDecoder::new(body), dest)
    } else if body.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
        let mut tar = Vec::new();
        lzma_rs::xz_decompress(&mut &body[..], &mut tar).map_err(|e| anyhow!("unpacking xz: {e}"))?;
        untar(&tar[..], dest)
    } else if body.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        let dec = ruzstd::decoding::StreamingDecoder::new(body).map_err(|e| anyhow!("unpacking zstd: {e}"))?;
        untar(dec, dest)
    } else if body.starts_with(b"PK\x03\x04") {
        zip::ZipArchive::new(Cursor::new(body)).and_then(|mut z| z.extract(dest)).context("unpacking zip")
    } else {
        let path = dest.join(name);
        fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        Ok(())
    }
}

fn untar(reader: impl Read, dest: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive.unpack(dest).context("unpacking tar")
}

/// First eight hex digits of the SHA-256 of the artifact's SRI string.
pub fn hash8(sri: &str) -> String {
    Sha256::digest(sri.as_bytes()).iter().take(4).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn is_exec(p: &Path) -> bool {
        fs::metadata(p).unwrap().permissions().mode() & 0o111 != 0
    }

    #[test]
    fn verifies_both_hash_algorithms() {
        let body = b"payload";
        verify(body, &sri_sha256(body)).unwrap();
        let sha512 = format!("sha512-{}", STANDARD.encode(Sha512::digest(body)));
        verify(body, &sha512).unwrap();
        let err = verify(b"other", &sri_sha256(body)).unwrap_err();
        assert!(err.to_string().contains("hash mismatch"), "{err}");
        assert!(verify(body, "md5-AAAA").is_err());
    }

    #[test]
    fn unpacks_xz_zstd_zip_and_raw() {
        let xz = include_bytes!("../tests/fixtures/hello.tar.xz");
        let zst = include_bytes!("../tests/fixtures/hello.tar.zst");
        for (label, body) in [("xz", &xz[..]), ("zst", &zst[..])] {
            let dir = tempfile::tempdir().unwrap();
            unpack(body, dir.path(), "hello").unwrap();
            assert!(is_exec(&dir.path().join("hello-1.0.0/hello")), "{label}");
        }

        let mut zip_bytes = Vec::new();
        {
            let mut z = zip::ZipWriter::new(Cursor::new(&mut zip_bytes));
            let opts = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
            z.start_file("hello", opts).unwrap();
            z.write_all(b"#!/bin/sh\n").unwrap();
            z.finish().unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        unpack(&zip_bytes, dir.path(), "hello").unwrap();
        assert!(is_exec(&dir.path().join("hello")));

        let dir = tempfile::tempdir().unwrap();
        unpack(b"\x7fELF raw", dir.path(), "tool").unwrap();
        assert!(is_exec(&dir.path().join("tool")));
    }
}
