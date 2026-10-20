# FormalSpec design

FormalSpec is a single Rust binary, `fspec`. It reads a config written in
Starlark, compares it with what it installed last time, and converges the
machine to match. Every version it installs is pinned in `fspec.lock`, which
lives in the same git repo as the config. Hosts that apply the same commit end
up with the same software.

## Goals

- Programmable, declarative config shared across Linux and macOS hosts.
- Identical hosts from one committed lock.
- Software available the day upstream ships it, with no central package repo
  in the way and no hashes written by hand.
- Self-updating apps keep working.
- Removing something from the config removes it from the machine, data
  included, with rollback.
- Runs on any distro; switching distros is a config change, not a USB stick.

Out of scope: building the world from source. Prebuilt upstream artifacts are
the default, and source builds happen only when a package needs patches.

## Config language: Starlark

Starlark is the Python dialect Bazel and Buck2 use for build files. FormalSpec
embeds it through the `starlark` crate, which Meta maintains for Buck2.

It won for two reasons. Agents and humans already write it fluently, since it
reads as Python. And it is deterministic by design: a config cannot read files,
call the network or look at the clock, so the same commit always evaluates to
the same plan. Lua was the runner-up; it has a bigger ecosystem, but the
determinism would have to be enforced by stripping its stdlib, and something
would eventually reach for `os.getenv`.

Host facts come from the `host()` declaration itself, never from the network,
so evaluation stays pure and `plan --all` works offline. `platform` is a
required argument (`"x86_64-linux"`, `"aarch64-darwin"`); `distro` and `init`
follow from `system` (`image(...)` gives the image's distro and systemd,
`fedora(...)`, `arch(...)`, `debian(...)` and `void(...)` give their own, and an
`aarch64-darwin` platform implies macOS and launchd). An entry in `modules` can
be a resource, a list of them, or a function, which FormalSpec calls with the
host's facts:

```python
def cli(host):
    tools = [pkg("ripgrep"), pkg("fd")]
    if host.platform.is_darwin:
        tools.append(pkg("aerospace", source = brew(cask = True)))
    return tools

hosts = [host("macbook", platform = "aarch64-darwin", modules = [cli])]
```

The host struct carries `name`, `distro`, `init` and `platform` (`name`, `os`,
`arch`, `is_darwin`, `is_linux`). `apply` probes the machine only to check that
it matches the declaration, and fails with a clear error when it does not.

`load("//modules/cli.star", "cli")` resolves from the directory holding
`spec.star`; there are no relative loads.

## Resources

A config evaluates to a list of resources:

| Resource | What it is |
| --- | --- |
| `host` | Name, `platform`, `system`, users, modules |
| `pkg` | A package from some source, pinned in the lock, with the files, services and timers that belong to it |
| `file` | A file FormalSpec writes, links, seeds or patches |
| `service` | A system or user service, rendered for the host's init |
| `user` | Account, UID, shell, groups |
| `secret` | An age-encrypted file decrypted at apply |
| `defaults` | A macOS `defaults` key |
| `image` | A bootc base image for the system layer |
| `timer` | A scheduled job (systemd timer, cron, launchd) |

Modules are plain Starlark functions that return lists of resources.

A package names the things that exist only for it, and they go when it goes:

```python
pkg("syncthing", source = github("syncthing/syncthing"),
    services = [service("syncthing", exec = "syncthing serve", user = True)],
    files = [file("~/.config/syncthing/config.xml", src = "files/syncthing.xml")])
```

A `file`, `service` or `timer` declared on its own is its own resource and is
removed when its own line disappears. Two declarations of the same file path,
package name or service name fail the plan.

## Package sources

| Source | Example |
| --- | --- |
| aqua registry (default for CLI tools) | `pkg("ripgrep")` |
| `github(repo)` | release assets matched per platform |
| `npm(name)` | `npm("@anthropic-ai/claude-code")` |
| `cargo(name)` | crates.io, prebuilt via cargo-binstall where available |
| `flatpak(id)` | Flathub |
| `brew(name)` | Homebrew bottles and casks on macOS |
| `dnf`, `pacman`, `apt`, `xbps` | the distro's own repos |
| `git(url, rev)` | build from source with a recipe |
| `nix(attr)` | borrow a nixpkgs build on hosts that have Nix |

`rev = "main"` and `"latest"` are resolved by `fspec update`, never at apply,
so the lock always holds an exact commit or version.

### Modes

- `pinned` (default): exact version and hash from the lock.
- `self_managed`: FormalSpec pins and installs the first version, then leaves
  the app's own updater alone. For Steam, Claude Code, browsers.
- `channel = "bleeding"` on a package skips any release-age delay a host
  sets as policy.

### Patches and build flags

A package with patches moves from prebuilt to source build:

```python
pkg("librepods",
    source = git("https://github.com/…/librepods"),
    patches = glob("patches/librepods-*.patch"),
    build_flags = ["--features", "pipewire"])
```

CI builds every patched package once per platform and pushes the result to an
OCI registry (ghcr) as an artifact. The lock records the digest, and hosts pull
it, so a weak machine never compiles. Patches on npm or script packages apply
straight to the unpacked files with no build step.

## The lock

`fspec.lock` is TOML, generated by `fspec update` and committed:

```toml
[[pkg]]
name = "ripgrep"
source = "aqua:BurntSushi/ripgrep"
version = "15.1.0"
x86_64-linux = { url = "https://github.com/BurntSushi/ripgrep/releases/…", hash = "sha256-…" }
aarch64-darwin = { url = "https://github.com/BurntSushi/ripgrep/releases/…", hash = "sha256-…" }

[[pkg]]
name = "librepods"
source = "git:https://github.com/…/librepods"
rev = "4f1c2a9…"
patches = "sha256-…"
x86_64-linux = { oci = "ghcr.io/formalsnake/fspec-cache/librepods@sha256:…" }

[system.g815]
image = "ghcr.io/ublue-os/bazzite-nvidia@sha256:…"
```

Distro repos usually keep only the newest version of a package, so FormalSpec
caches the exact `.rpm`, `.pkg.tar.zst` or `.xbps` files it locked. A pin
stays installable after the distro moves on.

A nightly CI job can run `fspec update` and open a PR with each bump and a
changelog link. Merging it is the auto-update.

## Store and generations

Packages from FormalSpec's own sources unpack into
`/opt/fspec/store/<hash>-<name>-<version>/`, never into `/usr`. A generation is
a directory of symlinks into the store, and `/opt/fspec/current` points at the
active one. On macOS the root is `/opt/fspec` too.

This gives FormalSpec three things Nix users expect:

- Switching is a single `rename(2)` of `current`, so it is atomic.
- Two versions of a tool can coexist; a project picks one with a
  `.fspec.toml` and a shell hook.
- `fspec rollback` repoints `current` at the previous generation.

Distro packages and Flatpaks live where their package manager puts them; the
state file still records them, so rollback reinstalls the previous set.

## Apply is a transaction

1. Fetch every artifact into the store and verify hashes. Nothing on the live
   system has changed yet, so a network failure aborts cleanly.
2. Render every file into a staging directory.
3. Write a journal of the intended changes.
4. Apply: files by write-then-rename, generation switch, package manager
   operations, then service reloads last.
5. On failure, replay the journal backwards. A crash mid-apply is resumed or
   reverted from the journal on the next run.

Before step 4, FormalSpec takes a btrfs or APFS snapshot of the paths it is
about to touch when the filesystem supports it.

## State

`/var/lib/fspec/state.json` (system) and `~/.local/state/fspec/state.json`
(user) record everything FormalSpec owns: each package and where it went, each
file with the hash it wrote, each service, each app data path it attributed to
a package, and the last-applied value of every key in a patched file. Plan is a
diff between the evaluated config and this state.

## Removal and cleanup

Deleting a line from the config removes the thing completely on the next
apply, the way removing a package from a NixOS config does, and goes further:
app data goes too.

When a `pkg` disappears from the config, apply:

1. Stops and removes the services and timers declared on it
   (`pkg(services = [...], timers = [...])`).
2. Uninstalls it (drops it from the generation, or asks the package manager).
3. Deletes the files declared on it (`pkg(files = [...])`), if they still match
   the hash it wrote. A file you or the app edited is kept and reported. Which
   module returned a file never ties it to a package; only `files = [...]` does.
4. Moves its app data to the generation trash.

App data is found three ways, in order:

- Declared on the package: `pkg("steam", data = ["~/.local/share/Steam"])`.
- Known by the source: Flatpak data under `~/.var/app/<id>`, registry metadata
  listing XDG paths for common apps.
- Attributed at install: after a package's first run, FormalSpec records new
  entries that appeared under `~/.config`, `~/.local/share`, `~/.local/state`
  and `~/.cache` whose names match the package. Anything it cannot attribute
  shows up in `fspec orphans` for you to claim or delete.

Trash lives under `/var/lib/fspec/trash/<generation>/` and
`~/.local/state/fspec/trash/<generation>/`. `fspec rollback` puts the data back
with the package. `fspec gc --older-than 14d` deletes old generations, their
store paths and their trash for good. A package can opt out with
`keep_data = True`.

## Files

Configs are generated from Starlark data and serialized to the target format
(JSON, TOML, YAML, INI, plist, Lua) or rendered from a template.

| Mode | On disk | When an app writes to it |
| --- | --- | --- |
| `managed` (default) | A real file, hash in state | `plan` shows the drift; `apply` overwrites unless `on_drift = "fail"`. `fspec adopt <path>` pulls the change back into the repo |
| `link` | Symlink into the repo working copy, stow style | The edit lands in the repo. If the app replaces the symlink with a file (an atomic rename), FormalSpec copies the content back and relinks |
| `seed` | Written once if missing | Never touched again |
| `patch` | A file the app owns; FormalSpec owns only the declared keys | Three-way merge, below |

Copies are the default, not symlinks: a copy survives apps that rename, chmod
or lock their config, and the hash in state detects drift just as well.

### Three-way merge for `patch`

```python
file("~/.claude/settings.json", mode = "patch", on_conflict = "ours", owns = {
    "model": "opus",
    "permissions.allow": list_union(["Bash(rg:*)", "Bash(fd:*)"]),
    "hooks.PostToolUse": list_keyed("matcher", hooks),
})
```

State keeps the value FormalSpec last wrote for each owned key, like
`kubectl apply`'s last-applied annotation. On apply, per key:

| On disk | Meaning | Result |
| --- | --- | --- |
| equals last-applied | untouched | write the new desired value |
| differs | the app or you changed it | `ours`: desired wins and plan shows it. `theirs`: keep it and stop managing the key until desired changes. `fail`: stop |
| missing | deleted | treated as a change |

Keys FormalSpec never declared are never read or written. Dropping a key from
`owns` deletes it only if it still holds the last-applied value.

Lists need a strategy: `list_union` (ensure these items, keep others),
`list_replace` (exact list) or `list_keyed(field)` (merge objects by a field).

`fspec watch` uses inotify or FSEvents to report drift on managed files as it
happens and stays quiet about writes to keys a patch does not own.

## Services and init systems

`service(...)` describes the service, not the init system:

```python
service("syncthing", exec = "syncthing serve --no-browser",
        user = True, restart = "always", after = ["network"])
```

| Init | Rendered as |
| --- | --- |
| systemd | unit files, user units under `~/.config/systemd/user` |
| runit (Void) | `/etc/sv/<name>/run`, linked into `/var/service`; user services under a per-user `runsvdir ~/.local/service` |
| OpenRC, dinit, s6 | their service scripts |
| launchd | plists in `/Library/LaunchDaemons` or `~/Library/LaunchAgents` |

A timer's `schedule` is a five-field cron expression or one of `@hourly`,
`@daily`, `@weekly` and `@monthly`. Timers become systemd timers, cron entries (snooze or cronie) or launchd
calendar intervals. Features only systemd has (socket activation, cgroup
limits, `graphical-session.target`) go under `systemd = {...}`; on another init
`plan` fails and says which field, rather than dropping it.

## System layer

The system layer is the kernel, bootloader, drivers and base packages.

On bootc distros (Fedora Atomic, Bazzite, Aurora, Bluefin, CentOS, and
community images for others) it is an image:

```python
system = image(
    base = "ghcr.io/ublue-os/bazzite-nvidia:stable",
    packages = dnf(["asusctl", "tuned-ppd"]),
    kernel_args = ["lockdown=integrity", "iommu=force"],
)
```

FormalSpec generates the Containerfile, CI builds and pushes it, the digest
goes in the lock, and apply runs `bootc switch` when it changes. Rollback is
the previous deployment in the boot menu.

On classic distros (Fedora Workstation, Arch, Debian, Void) it is
`fedora(...)`, `arch(...)` and so on, driving the package manager directly with
cached pinned packages, and kernel rollback comes from btrfs snapshots.

macOS has no equivalent of either; the system layer there is `defaults`,
launchd and Homebrew.

### Secure Boot and signed kernels

Stick with distros whose kernels carry well-known signing keys when a game's
anti-cheat requires it. Epic's EAC Linux v2 trust list (October 2026) includes
Fedora, Universal Blue, Canonical, Debian, openSUSE and Rocky, but not Arch,
CachyOS or NixOS. `image(...)` sets the required kernel arguments; firmware
settings (Secure Boot, TPM) stay manual.

## Switching distros

Personal files are never managed. FormalSpec only touches paths it declared or
attributed to a package, and `protect = ["/home", "/var/home"]` makes it refuse
any action under those paths. The spec pins your UID so ownership survives a
hop.

Between bootc distros, change `image(base = ...)` and apply. FormalSpec
rebuilds your image on the new base and runs `bootc switch`; you reboot into
the new distro with the old one still in the boot menu. Home, Flatpaks, store
packages and user services are outside the image and carry over.
`fspec switch --reset-var` resets `/etc` and `/var` to the new image's
defaults, except paths listed in `persist`.

To or from a classic distro, `fspec switch --distro void`:

1. Creates a new btrfs root subvolume.
2. Bootstraps the base system into it from the running OS (`dnf --installroot`,
   `pacstrap`, `xbps-install -r`, `debootstrap`).
3. Applies the spec inside it with the new distro's backends.
4. Adds a boot entry. `/home` is a shared subvolume.

The old root stays bootable until `fspec forget <distro>` deletes it, which also
deletes every leftover system file it had. Getting a non-bootc machine onto a
bootc image uses `bootc install to-existing-root`.

Only the `system` block differs per distro. Everything from FormalSpec's own
sources, Flatpaks and files moves unchanged. Base packages are named per
distro, so the `system` block is where those names live.

## Commands

| Command | Does |
| --- | --- |
| `fspec update [pkg]` | Resolve upstream, rewrite the lock |
| `fspec plan [--all]` | Diff config against state, per host |
| `fspec apply [--host h] [--all]` | Converge, over ssh for remote hosts |
| `fspec rollback [gen]` | Previous generation, data included |
| `fspec gc --older-than 14d` | Drop old generations, store paths and trash |
| `fspec adopt <path>` | Pull an app's edit of a managed file into the repo |
| `fspec orphans` | Unattributed app data |
| `fspec watch` | Report drift live |
| `fspec switch --distro d` | Install another distro beside this one |
| `fspec forget <distro>` | Delete an old distro root |

## Repo layout

```
spec.star          entry point, returns `hosts`
modules/           one concern per file
files/             static config files
patches/           .patch files referenced by packages
fspec.lock         generated, committed
```

## Implementation

A cargo workspace under `crates/`:

| Crate | Holds |
| --- | --- |
| `fspec-core` | Resource types, host facts, `Paths`, the state file, the lock, the plan diff, the apply engine, the journal, the `Runner` and `Fetch` seams |
| `fspec-eval` | Starlark builtins and evaluation into `Host` values |
| `fspec` | The CLI |

Builtins validate their arguments against the `fspec-core` types as soon as
they run, so an unknown field fails at the line that wrote it. State keys are
`kind:id` (`pkg:ripgrep`, `file:~/.config/fish/config.fish`); each entry keeps
the resource as last applied, the package that owns it, and a `record` object
whose shape belongs to the backend that applied it.

Backends implement one trait: `handles` claims resources on a host (exactly
one backend per resource, or `plan` lists it as unsupported and `apply`
refuses), `drift` reports live divergence for unchanged resources, `prepare`
fetches and renders into a staging directory, and `commit` makes changes live,
journaling an undo step before each mutation. The engine commits removals in
reverse phase order, then adds and updates in phase order (users, files, store,
packages, services, defaults, system), and writes the new state and
`generations/<n>.json` only after every commit succeeded. Each scope (system,
user) is its own transaction with its own journal. A non-root `fspec apply`
runs the system scope through `sudo fspec apply --scope system`, then the user
scope as the user. Every external command goes through a `Runner` and every
HTTP read through `Fetch`, so tests record them instead of touching the
machine or the network. With `--root` set to anything but `/`, the CLI logs
commands to `<root>/var/log/fspec-commands.log` instead of running them, and
`Fetch` also reads `file://` URLs for fixtures.

Every path goes through `Paths`, rooted at `--root` (default `/`) with the
home at `--home`, so tests and dry runs work in a temp directory and never
touch the live system.

## Build order

1. Starlark evaluation, resources, state file, `plan`.
2. `file` with all four modes, three-way merge for `patch`.
3. Store, generations, rollback; aqua, github and npm sources; `update` and the
   lock.
4. Flatpak, brew, dnf backends; systemd and launchd services.
5. Removal with data trash and `gc`.
6. Remote apply over ssh.
7. `image` with bootc, the OCI cache for patched builds.
8. Other distros and inits, `fspec switch`.

Steps 1 to 4 replace a home-manager setup on a single machine and are the
point where FormalSpec becomes usable day to day.
