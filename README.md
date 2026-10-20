```

 ▄▄▄▄▄▄                             ▀▀█     ▄▄▄▄
 █       ▄▄▄    ▄ ▄▄  ▄▄▄▄▄   ▄▄▄     █    █▀   ▀ ▄▄▄▄    ▄▄▄    ▄▄▄
 █▄▄▄▄▄ █▀ ▀█   █▀  ▀ █ █ █  ▀   █    █    ▀█▄▄▄  █▀ ▀█  █▀  █  █▀  ▀
 █      █   █   █     █ █ █  ▄▀▀▀█    █        ▀█ █   █  █▀▀▀▀  █
 █      ▀█▄█▀   █     █ █ █  ▀▄▄▀█    ▀▄▄  ▀▄▄▄█▀ ██▄█▀  ▀█▄▄▀  ▀█▄▄▀
                                                  █
                                                  ▀
```

[![Rust](https://img.shields.io/badge/built_with-rust-4a9eda?style=for-the-badge&labelColor=161616&logo=rust&logoColor=white)](docs/design.md)
[![Config](https://img.shields.io/badge/config-starlark-4a9eda?style=for-the-badge&labelColor=161616)](docs/design.md#config-language-starlark)
[![Status](https://img.shields.io/badge/status-untested-d3a35f?style=for-the-badge&labelColor=161616)](docs/design.md#build-order)
[![License](https://img.shields.io/badge/license-MIT-cccccc?style=for-the-badge&labelColor=161616)](LICENSE)

Declare your machines in code and keep them identical, on any Linux distro
and on macOS. You write the config in Starlark, FormalSpec pins every version
in one lockfile, and each host that applies the same commit ends up with the
same software.

FormalSpec doesn't use a central package repository. Packages come straight
from upstream: GitHub releases, npm, Flathub, crates.io, Homebrew and your
distro's own repos. When you run `fspec update`, it finds new releases and
writes exact versions and hashes into `fspec.lock`, so you never type a hash or
wait for someone to bump a package. Apps that update themselves, like Steam and
Claude Code, keep doing that.

```
 spec.star + fspec.lock                     each host
┌──────────────────────┐   fspec apply   ┌──────────────────────────┐
│ hosts, modules,      │ ──────────────▶ │ store + generations      │
│ packages, files,     │   (ssh for      │ files, services, users   │
│ services             │    remote)      │ bootc image or distro    │
└──────────────────────┘                 └──────────────────────────┘
```

**Untested on real hosts.** Every step of the build order is implemented and
covered by tests that run against temp roots, with package managers, init
systems and bootc calls recorded instead of run. Nothing has converged a live
machine yet. [`docs/design.md`](docs/design.md) is the spec.

## What it manages

| | |
|---|---|
| Packages | Prebuilt from upstream, pinned by hash. Patched packages build once in CI and download as OCI artifacts |
| Files | Written, linked, seeded once, or merged key by key into a file the app also writes |
| Services | One description, rendered for systemd, runit, OpenRC, dinit, s6 or launchd |
| System | A bootc image on Fedora Atomic, Bazzite and friends, or the distro's package manager elsewhere |
| Cleanup | Removing a line removes the package, its services, its files and its app data, and rollback brings them back |
| Distros | Change the base image and apply, or install a second distro beside the first without a USB stick |

## Usage

```
fspec update            # find new releases, rewrite fspec.lock
fspec plan              # show what would change on this host
fspec apply             # converge this host
fspec apply --all       # converge every host over ssh
fspec rollback          # go back to the previous generation
fspec gc --older-than 14d
```

## Config

`spec.star` returns your hosts. A module is a function that returns packages,
files and services.

```python
load("//modules/cli.star", "cli")

def laptop(name, extra = []):
    return host(
        name = name,
        platform = "x86_64-linux",
        system = image(base = "ghcr.io/ublue-os/bazzite-nvidia:stable"),
        users = [user("kyandesutter", uid = 1000, shell = "fish")],
        modules = [cli()] + extra,
    )

hosts = [laptop("g815"), laptop("e1504g")]
```

```python
def cli():
    return [
        pkg("ripgrep"),
        pkg("claude-code", source = npm("@anthropic-ai/claude-code"),
            mode = "self_managed"),
        file("~/.config/fish/config.fish", src = "files/config.fish"),
        file("~/.claude/settings.json", mode = "patch",
             owns = {"model": "opus"}),
    ]
```

[`docs/design.md`](docs/design.md) covers the full model.
[`docs/from-nix.md`](docs/from-nix.md) compares FormalSpec with Nix and shows
how a NixOS or nix-darwin setup maps over.

## License

MIT
