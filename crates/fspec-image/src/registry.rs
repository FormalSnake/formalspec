use anyhow::{Context, Result, bail};
use fspec_core::fetch::Fetch;
use fspec_core::lock::{Lock, LockedSystem};
use fspec_core::resource::{Host, System};
use sha2::{Digest, Sha256};

use crate::TAG;

const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.index.v1+json, \
    application/vnd.docker.distribution.manifest.list.v2+json, \
    application/vnd.oci.image.manifest.v1+json, \
    application/vnd.docker.distribution.manifest.v2+json";

/// The repository CI pushes a host's image to: `<registry>/fspec-<host>`, lowercase as ghcr requires.
pub fn repo(registry: &str, host: &str) -> Result<String> {
    let name = host.to_ascii_lowercase();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
        bail!("host name `{host}` cannot be part of an image repository name");
    }
    Ok(format!("{}/fspec-{name}", registry.trim_end_matches('/')).to_ascii_lowercase())
}

/// The `sha256:…` digest `tag` points at in an anonymous-pull ghcr repository (`ghcr.io/owner/name`).
pub fn resolve_digest(fetch: &dyn Fetch, repository: &str, tag: &str) -> Result<String> {
    let Some(path) = repository.strip_prefix("ghcr.io/") else {
        bail!("{repository}: only ghcr.io registries are supported");
    };
    let hint = || format!("resolving {repository}:{tag} (the ghcr package must be public for anonymous pulls)");
    let token_url = format!("https://ghcr.io/token?scope=repository:{path}:pull");
    let token = fetch.get_json(&token_url).with_context(hint)?;
    let token = token["token"].as_str().with_context(|| format!("{token_url} returned no token")).with_context(hint)?;
    let url = format!("https://ghcr.io/v2/{path}/manifests/{tag}");
    let auth = format!("Bearer {token}");
    let resp = fetch.request(&url, &[("Authorization", &auth), ("Accept", MANIFEST_ACCEPT)]).with_context(hint)?;
    let digest = match resp.header("docker-content-digest") {
        Some(d) => d.trim().to_string(),
        None => format!("sha256:{}", hex(&Sha256::digest(&resp.body))),
    };
    let valid =
        digest.strip_prefix("sha256:").is_some_and(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()));
    if !valid {
        bail!("{url} reported the digest `{digest}`, which is not sha256:<64 hex digits>");
    }
    Ok(digest)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub struct ImageUpdate {
    pub lock: Lock,
    /// One line per change, e.g. `image g815 sha256:aaaa -> sha256:bbbb`.
    pub changes: Vec<String>,
    /// Hosts whose digest failed to resolve; their old entries stay in `lock`.
    pub errors: Vec<String>,
}

/// Pin every image host's pushed tag in `[system.<host>]`, and drop entries for hosts that no
/// longer declare an image. Package entries pass through untouched.
pub fn update_images(hosts: &[Host], old: &Lock, registry: &str, fetch: &dyn Fetch) -> Result<ImageUpdate> {
    let mut lock = Lock { pkg: old.pkg.clone(), system: Default::default() };
    let (mut changes, mut errors) = (Vec::new(), Vec::new());
    for host in hosts {
        let before = old.system.get(&host.name);
        if !matches!(host.system, Some(System::Image(_))) {
            if let Some(b) = before {
                changes.push(format!("- image {} {}", host.name, b.image));
            }
            continue;
        }
        let pinned = repo(registry, &host.name).and_then(|r| {
            let digest = resolve_digest(fetch, &r, TAG).with_context(|| format!("host {}", host.name))?;
            Ok((r, digest))
        });
        match pinned {
            Ok((r, digest)) => {
                let image = format!("{r}@{digest}");
                match before {
                    None => changes.push(format!("+ image {} {image}", host.name)),
                    Some(b) if b.image != image => {
                        changes.push(format!("image {} {} -> {digest}", host.name, short(&b.image)))
                    }
                    Some(_) => {}
                }
                lock.system.insert(host.name.clone(), LockedSystem { image });
            }
            Err(e) => {
                errors.push(format!("{e:#}"));
                if let Some(b) = before {
                    lock.system.insert(host.name.clone(), b.clone());
                }
            }
        }
    }
    Ok(ImageUpdate { lock, changes, errors })
}

fn short(image: &str) -> &str {
    image.rsplit_once('@').map_or(image, |(_, d)| d)
}

#[cfg(test)]
mod tests {
    use fspec_core::fetch::MapFetch;
    use fspec_core::resource::Image;

    use super::*;

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const TOKEN_URL: &str = "https://ghcr.io/token?scope=repository:formalsnake/fspec-g815:pull";
    const MANIFEST_URL: &str = "https://ghcr.io/v2/formalsnake/fspec-g815/manifests/latest";

    fn registry(digest_header: Option<&str>) -> MapFetch {
        let f = MapFetch::default()
            .with(TOKEN_URL, r#"{"token":"tok","expires_in":300}"#)
            .with(MANIFEST_URL, "{\"manifests\":[]}");
        match digest_header {
            Some(d) => f.with_headers(MANIFEST_URL, &[("docker-content-digest", d)]),
            None => f,
        }
    }

    fn image_host(name: &str) -> Host {
        let image =
            Image { base: "ghcr.io/ublue-os/bazzite:stable".into(), packages: None, kernel_args: vec![], distro: None };
        Host {
            name: name.into(),
            platform: "x86_64-linux".into(),
            system: Some(System::Image(image)),
            users: vec![],
            resources: vec![],
            protect: vec![],
            persist: vec![],
            release_age: None,
        }
    }

    #[test]
    fn token_then_manifest_digest_header() {
        let f = registry(Some(DIGEST));
        assert_eq!(resolve_digest(&f, "ghcr.io/formalsnake/fspec-g815", "latest").unwrap(), DIGEST);
        let sent = f.requests.lock().unwrap();
        assert_eq!(sent.len(), 1);
        let (url, headers) = &sent[0];
        assert_eq!(url, MANIFEST_URL);
        assert!(headers.contains(&("Authorization".into(), "Bearer tok".into())));
        let accept = &headers.iter().find(|(k, _)| k == "Accept").unwrap().1;
        assert!(accept.contains("application/vnd.oci.image.index.v1+json"));
        assert!(accept.contains("application/vnd.docker.distribution.manifest.list.v2+json"));
    }

    #[test]
    fn digest_falls_back_to_the_body_hash() {
        let f = registry(None);
        let got = resolve_digest(&f, "ghcr.io/formalsnake/fspec-g815", "latest").unwrap();
        let want = format!("sha256:{}", hex(&Sha256::digest(b"{\"manifests\":[]}")));
        assert_eq!(got, want);
    }

    #[test]
    fn bad_registries_and_digests_fail() {
        let f = registry(Some("md5:abc"));
        let err = resolve_digest(&f, "ghcr.io/formalsnake/fspec-g815", "latest").unwrap_err();
        assert!(format!("{err:#}").contains("not sha256"), "{err:#}");
        let err = resolve_digest(&f, "quay.io/x/y", "latest").unwrap_err();
        assert!(err.to_string().contains("only ghcr.io"), "{err}");
        let err = resolve_digest(&MapFetch::default(), "ghcr.io/formalsnake/fspec-g815", "latest").unwrap_err();
        assert!(format!("{err:#}").contains("must be public"), "{err:#}");
    }

    #[test]
    fn update_writes_system_entries_and_reports_changes() {
        let f = registry(Some(DIGEST));
        let mac = Host { system: None, platform: "aarch64-darwin".into(), ..image_host("macbook") };
        let hosts = [image_host("g815"), mac];
        let old = Lock::default();
        let u = update_images(&hosts, &old, "ghcr.io/FormalSnake", &f).unwrap();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
        assert_eq!(u.lock.system["g815"].image, format!("ghcr.io/formalsnake/fspec-g815@{DIGEST}"));
        assert_eq!(u.changes, [format!("+ image g815 ghcr.io/formalsnake/fspec-g815@{DIGEST}")]);

        let again = update_images(&hosts, &u.lock, "ghcr.io/FormalSnake", &f).unwrap();
        assert!(again.changes.is_empty());
        assert_eq!(again.lock, u.lock);
    }

    #[test]
    fn update_keeps_the_old_pin_when_resolution_fails() {
        let mut old = Lock::default();
        old.system.insert("g815".into(), LockedSystem { image: "ghcr.io/o/fspec-g815@sha256:old".into() });
        let u = update_images(&[image_host("g815")], &old, "ghcr.io/o", &MapFetch::default()).unwrap();
        assert_eq!(u.errors.len(), 1);
        assert_eq!(u.lock, old);
    }
}
