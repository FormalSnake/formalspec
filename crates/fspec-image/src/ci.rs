use std::path::Path;

use anyhow::{Context, Result, bail};
use fspec_core::runner::{Cmd, Runner};

use crate::{TAG, repo};

/// Where `fspec ci init` writes the workflow, relative to the spec's directory.
pub const WORKFLOW_PATH: &str = ".github/workflows/fspec-images.yml";

/// `ghcr.io/<owner>` from the `origin` remote of the repository holding `dir`.
pub fn default_registry(runner: &dyn Runner, dir: &Path) -> Result<String> {
    let cmd = Cmd::new("git").arg("-C").arg(dir.display().to_string()).args(["remote", "get-url", "origin"]);
    let out = runner.run(&cmd)?;
    let url = out.stdout.trim();
    if out.status != 0 || url.is_empty() {
        bail!("cannot read the git remote `origin` in {}; pass --registry ghcr.io/<owner>", dir.display());
    }
    let owner = url
        .split_once("github.com")
        .and_then(|(_, rest)| rest.trim_start_matches([':', '/']).split('/').next())
        .filter(|o| !o.is_empty())
        .with_context(|| format!("`{url}` is not a github.com remote; pass --registry ghcr.io/<owner>"))?;
    Ok(format!("ghcr.io/{}", owner.to_ascii_lowercase()))
}

/// The workflow that builds each host's image, pushes it to `<registry>/fspec-<host>` and
/// opens a PR with the new digests in `fspec.lock`.
pub fn workflow(registry: &str, spec: &str, hosts: &[String]) -> Result<String> {
    if hosts.is_empty() {
        bail!("no host declares an image(...), so there is nothing to build");
    }
    for h in hosts {
        repo(registry, h)?;
    }
    let registry = registry.trim_end_matches('/').to_ascii_lowercase();
    let matrix = hosts.join(", ");
    let source = env!("CARGO_PKG_REPOSITORY");
    Ok(format!(
        r#"name: fspec-images

on:
  push:
    branches: [main]
    paths: ["**/*.star", "files/**", "patches/**"]
  schedule:
    - cron: "0 4 * * 1"
  workflow_dispatch:

permissions:
  contents: write
  packages: write
  pull-requests: write

env:
  REGISTRY: {registry}
  SPEC: {spec}
  FSPEC_SOURCE: {source}

jobs:
  build:
    runs-on: ubuntu-latest
    strategy:
      fail-fast: false
      matrix:
        host: [{matrix}]
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo install --locked --git "$FSPEC_SOURCE" fspec
      - name: Render the Containerfile
        run: fspec --spec "$SPEC" image containerfile --host "${{{{ matrix.host }}}}" > "$RUNNER_TEMP/Containerfile"
      - name: Build
        run: podman build --file "$RUNNER_TEMP/Containerfile" --tag "$REGISTRY/fspec-${{{{ matrix.host }}}}:{TAG}" .
      - name: Push
        run: |
          echo "${{{{ secrets.GITHUB_TOKEN }}}}" | podman login ghcr.io --username "${{{{ github.actor }}}}" --password-stdin
          podman push "$REGISTRY/fspec-${{{{ matrix.host }}}}:{TAG}"

  lock:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo install --locked --git "$FSPEC_SOURCE" fspec
      - run: fspec --spec "$SPEC" update --images --registry "$REGISTRY"
      - uses: peter-evans/create-pull-request@v7
        with:
          add-paths: fspec.lock
          branch: fspec/image-digests
          commit-message: "chore(image): update image digests"
          title: "chore(image): update image digests"
          body: Digests from the fspec-images workflow.
"#
    ))
}

#[cfg(test)]
mod tests {
    use fspec_core::runner::{Output, RecordingRunner};

    use super::*;
    use crate::testutil::golden;

    #[test]
    fn workflow_golden() {
        let hosts = ["e1504g".to_string(), "g815".to_string()];
        golden("fspec-images.yml", &workflow("ghcr.io/formalsnake", "spec.star", &hosts).unwrap());
    }

    #[test]
    fn workflow_needs_an_image_host() {
        assert!(workflow("ghcr.io/o", "spec.star", &[]).is_err());
    }

    fn remote(url: &str) -> RecordingRunner {
        let r = RecordingRunner::default();
        r.respond("git -C", Output { status: 0, stdout: format!("{url}\n"), stderr: String::new() });
        r
    }

    #[test]
    fn registry_comes_from_the_git_remote() {
        for url in [
            "git@github.com:FormalSnake/formalspec.git",
            "https://github.com/FormalSnake/formalspec",
            "ssh://git@github.com/FormalSnake/formalspec.git",
        ] {
            assert_eq!(default_registry(&remote(url), Path::new("/repo")).unwrap(), "ghcr.io/formalsnake", "{url}");
        }
        let err = default_registry(&remote("git@gitlab.com:o/r.git"), Path::new("/repo")).unwrap_err();
        assert!(err.to_string().contains("--registry"), "{err}");
        let err = default_registry(&RecordingRunner::default(), Path::new("/repo")).unwrap_err();
        assert!(err.to_string().contains("origin"), "{err}");
    }
}
