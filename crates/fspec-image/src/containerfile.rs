use anyhow::{Result, bail};
use fspec_core::resource::{Image, Source};
use serde::Serialize;

const KARGS_FILE: &str = "/usr/lib/bootc/kargs.d/fspec.toml";

/// Distros whose images carry no RPM database, so `dnf(...)` cannot install into them.
const NON_RPM: [&str; 8] = ["arch", "archlinux", "cachyos", "debian", "ubuntu", "void", "alpine", "nixos"];

#[derive(Serialize)]
struct Kargs<'a> {
    kargs: &'a [String],
}

/// The Containerfile CI builds for an `image(...)`: the base, the layered packages, the kernel
/// arguments as a `kargs.d` drop-in, and the bootc lint bootc recommends as the last step.
pub fn containerfile(image: &Image) -> Result<String> {
    let mut out = format!("FROM {}\n", line(&image.base, "base")?);
    if let Some(source) = &image.packages {
        let Source::Dnf(d) = source else {
            bail!("image packages must be dnf(...), got `{source}`: bootc images layer packages with dnf")
        };
        let distro = image.distro();
        if NON_RPM.contains(&distro.as_str()) {
            bail!("image packages use dnf(...) but the base `{}` is a {distro} image, which has no dnf", image.base);
        }
        if d.names.is_empty() {
            bail!("image packages: dnf(...) needs package names");
        }
        for n in &d.names {
            if n.is_empty() || !n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-')) {
                bail!("image packages: `{n}` is not a package name");
            }
        }
        out.push_str(&format!("RUN dnf install -y {} && dnf clean all\n", d.names.join(" ")));
    }
    if !image.kernel_args.is_empty() {
        for a in &image.kernel_args {
            if a.is_empty() || a.contains(['\'', '\n', '\r']) {
                bail!("kernel argument `{}` must be non-empty, single line and hold no single quote", a.escape_debug());
            }
        }
        let toml = toml::to_string(&Kargs { kargs: &image.kernel_args })?;
        let toml = toml.trim_end();
        let dir = KARGS_FILE.rsplit_once('/').map_or("", |(d, _)| d);
        out.push_str(&format!("RUN mkdir -p {dir} && printf '%s\\n' '{toml}' > {KARGS_FILE}\n"));
    }
    out.push_str("RUN bootc container lint\n");
    Ok(out)
}

fn line<'a>(v: &'a str, what: &str) -> Result<&'a str> {
    if v.is_empty() || v.contains(char::is_whitespace) {
        bail!("image {what} `{}` must be one reference without whitespace", v.escape_debug());
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use fspec_core::resource::DistroPkgs;

    use super::*;
    use crate::testutil::golden;

    fn image(base: &str, packages: Option<Source>, kargs: &[&str]) -> Image {
        Image { base: base.into(), packages, kernel_args: kargs.iter().map(|s| s.to_string()).collect(), distro: None }
    }

    fn dnf(names: &[&str]) -> Option<Source> {
        Some(Source::Dnf(DistroPkgs { names: names.iter().map(|s| s.to_string()).collect() }))
    }

    #[test]
    fn full_image() {
        let i = image(
            "ghcr.io/ublue-os/bazzite-nvidia:stable",
            dnf(&["asusctl", "tuned-ppd"]),
            &["lockdown=integrity", "iommu=force"],
        );
        golden("containerfile-full.txt", &containerfile(&i).unwrap());
    }

    #[test]
    fn bare_base_only_lints() {
        let i = image("quay.io/fedora/fedora-silverblue:42", None, &[]);
        golden("containerfile-bare.txt", &containerfile(&i).unwrap());
    }

    #[test]
    fn kargs_only() {
        let i = image("ghcr.io/ublue-os/aurora:stable", None, &["iommu=force"]);
        golden("containerfile-kargs.txt", &containerfile(&i).unwrap());
    }

    #[test]
    fn rejects_managers_the_base_cannot_use() {
        let pacman = Some(Source::Pacman(DistroPkgs { names: vec!["htop".into()] }));
        let err = containerfile(&image("ghcr.io/ublue-os/bazzite:stable", pacman, &[])).unwrap_err();
        assert!(err.to_string().contains("must be dnf"), "{err}");
        let err = containerfile(&image("docker.io/archlinux/archlinux:latest", dnf(&["htop"]), &[])).unwrap_err();
        assert!(err.to_string().contains("no dnf"), "{err}");
        let err = containerfile(&image("ghcr.io/x/fedora-bootc", dnf(&["a b"]), &[])).unwrap_err();
        assert!(err.to_string().contains("not a package name"), "{err}");
        let err = containerfile(&image("ghcr.io/x/fedora-bootc", None, &["it's"])).unwrap_err();
        assert!(err.to_string().contains("kernel argument"), "{err}");
    }
}
