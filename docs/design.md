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
so evaluation stays pure. `platform` is a
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
  sets as policy (`host(release_age = "3d")`). A package resolves to the newest
  release at least that old, using the longest delay among the hosts that
  declare it.

### Patches and build flags

A package with patches moves from prebuilt to source build:

```python
pkg("librepods",
    source = git("https://github.com/…/librepods"),
    patches = glob("patches/librepods-*.patch"),
    build_flags = ["--features", "pipewire"])
```

`glob(pattern)` returns the sorted, repo-relative paths of the files under the
spec's directory that match. It is the one place evaluation reads the
filesystem besides `load`, and it only reads the spec's own tree: patterns
cannot start with `/` or contain `..`, and links that lead outside the tree are
dropped.

`fspec update` writes `patches = "sha256-…"` into the lock: a hash over the
patch file names and contents, in the order the spec lists them. A package
whose patches no longer hash to the lock's value is refused ("patches changed,
run fspec update"), at `apply` and as a drift note in `plan`.

Fields on `pkg` for source builds: `patches`, `build_flags` (appended to the
recipe's configure or build command), `build = ["sh", "-c", "…"]` (replaces
recipe detection) and `bins = ["name", …]` (the executables to ship, by file
name; default is every executable the recipe produced).

**Recipes.** `fspec build` checks out the locked commit, applies the patches
with `git apply`, then picks the first match: `Cargo.toml`, `meson.build`,
`CMakeLists.txt`, `go.mod`, a `Makefile`. An explicit `build` wins over all of
them.

| Recipe | Commands |
| --- | --- |
| Cargo | `cargo build --release --locked --manifest-path <src>/Cargo.toml --target-dir <build> <build_flags>`; executables come from `<build>/release` |
| Meson | `meson setup <build> <src> --buildtype release --prefix <out> <flags>`, `meson compile -C <build>`, `meson install -C <build>` |
| CMake | `cmake -S <src> -B <build> -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=<out> <flags>`, `cmake --build <build> --parallel`, `cmake --install <build>` |
| Go | `go build -C <src> -o <out>/bin/ <flags> ./...` |
| Make | `make -C <src> <flags>`, `make -C <src> install PREFIX=<out>` |
| explicit | the `build` command, with `FSPEC_SRC` (the patched checkout) and `FSPEC_OUT` (put executables in `$FSPEC_OUT/bin`) in its environment; `build_flags` do not apply |

Every command goes through the `Runner`. Non-Cargo recipes install into
`<out>`, so executables are read from `<out>/bin`.

**Commands.**

- `fspec build --pkg <name> --platform <p> --out <dir>` builds on this
  machine, which must be platform `<p>`. It writes
  `<dir>/<name>-<p>.tar.zst` with the executables at the top (sorted names,
  zeroed owners and times) and `<dir>/tag` holding `<rev>-<p>`.
- `fspec ci packages` writes `.github/workflows/fspec-packages.yml` next to the
  spec: a matrix with one job per `git(...)` package and platform that a host
  declares (`ubuntu-24.04` for x86_64-linux, `ubuntu-24.04-arm` for
  aarch64-linux, `macos-14` for aarch64-darwin). Each job installs fspec and
  oras, runs `fspec build` and pushes with `oras push`. Regenerate it when the
  set of packages or platforms changes; the lock and `patches/` changes trigger
  it.
- `fspec update --cache [--registry r]` also resolves
  `<registry>/<name>:<rev>-<platform>` with
  `oras manifest fetch --descriptor` and records
  `<platform> = { oci = "<registry>/<name>@sha256:…" }`. The registry defaults
  to `ghcr.io/<origin owner>/fspec-cache`. A plain `fspec update` keeps the
  recorded digests while the commit and the patches are unchanged.

**Registry layout.** One repository per package,
`ghcr.io/<owner>/fspec-cache/<name>`, one tag per commit and platform
(`<rev>-<platform>`), each a one-layer artifact whose layer is the `.tar.zst`
(media type `application/vnd.fspec.package.v1.tar+zstd`, file name as its
title). The lock pins the manifest digest.

**On the host.** A package whose locked artifact has `oci` is pulled with
`oras pull <ref> -o <staging>` through the `Runner` (so `oras` must be on
PATH), the layer file is hashed against the layer digest in
`oras manifest fetch <ref>`, unpacked, and installed like any other store
package. A reference without a digest is refused. The host never compiles.

Patches on npm and other packages that ship as an unpacked tree need no build:
`prepare` applies them in pure Rust (the `diffy` crate: git or unified diffs
with `a/` and `b/` prefixes, text edits, file creation, deletion and rename)
to the package directory (`package/` for npm, `node_modules/<name>` after an
`npm install`, else the archive's single top-level directory) before the tree
gets its store path. The store directory name hashes the artifact hash together
with the patches hash, so a patched tree never shares a path with the
unpatched one.

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

`fspec update [pkg...]` resolves every package declared by any host (or only
the named ones), keeps the other entries, drops entries no host declares and
prints one line per change. A package gets an artifact for each platform its
hosts declare. A distro source (`dnf`, `pacman`, `apt`, `xbps`) records the
version plus the exact package file URL and hash per platform, found two ways.
When the machine running `fspec update` is that distro on that arch, it asks its
own package manager: `dnf repoquery --location`, `pacman -Sddp`,
`apt-get download --print-uris`, `xbps-query -R --property=...`. Otherwise
Arch reads the `core.db` and `extra.db` tarballs from `geo.mirror.pkgbuild.com`
(x86_64 only; Arch Linux ARM has its own mirrors) and Void reads
`<arch>-repodata` from `repo-default.voidlinux.org`. Fedora and Debian have no
repo metadata simple enough to parse here, so on any other machine those
platforms stay name-only: the host installs the newest version by name and
nothing is cached. Run `fspec update` on a Fedora or Debian machine of the
right arch to get exact pins; `release_age` does not apply to distro sources.
The lock entry for a distro package holds one artifact per platform, so the
cached file is used for single-name packages; a multi-name `dnf([...])` package
installs by name. Flatpak pins live in `any = { commit = … }`, brew records
`version`; Homebrew can only install the current version, so the brew pin is
checked (drift, then `brew upgrade`), not enforced, and an upgrade is not
reversed on rollback.

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

Users put `/opt/fspec/current/bin` on PATH; nothing else exposes store tools.

A generation is `generations/<n>/bin/`, symlinks into the store (relative, so a
rooted tree stays movable). Each apply starts from the links of the generation
`current` points at, applies only the changed packages, and renames a temp
symlink over `current` last; the journal restores the old link if anything
fails. Which executables get linked: the lock artifact's `bins` when set, else
every executable regular file in the top two levels of the unpacked tree; for
npm, the package's `bin` entries. Store directories are named
`<hash8>-<name>-<version>`, never rewritten, and reused when present. An
artifact is verified against its `sha256-` or `sha512-` SRI hash before
anything is unpacked (`.tar.gz`, `.tar.xz`, `.tar.zst`, `.zip`, or a bare
binary named after the package).

Each pkg's record keeps the store path, version, linked bins and the lock entry
it was installed from. `fspec rollback [gen]` re-applies that generation's
resources with a lock rebuilt from those records, as a new generation, so the
old store paths are reused and no download happens. The live `fspec.lock` is
untouched: until it is reverted too, `plan` reports the newer locked version as
drift. A `pinned` pkg whose locked artifact differs from the record is drift
(that is how a bumped lock reaches `apply`); a `self_managed` pkg is installed
once and never touched by later lock changes.

npm packages are the registry tarball. One without runtime `dependencies` is
unpacked and its `bin` linked; one with dependencies is installed with
`npm install --omit=dev --prefix <dir>`, so the host needs npm on PATH. The
`nix` source runs `nix build --no-link --print-out-paths nixpkgs#<attr>` (pinned
to the lock's `rev` when set) and links the result's `bin/`; nothing is hashed or
copied.

`.fspec.toml` in a project pins versions:

```toml
[tools]
ripgrep = "14.1.0"
```

`eval "$(fspec shell-hook zsh)"` (also `bash`, and `fspec shell-hook fish | source`)
installs a hook that runs `fspec env --dir $PWD` on directory change and puts
the bins of each pinned version that is in the store first on PATH. The hook
shells out to a hidden `fspec env`, which links them under
`~/.cache/fspec/pins/`.

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
`~/.local/state/fspec/trash/<generation>/`, chosen by where the path lives, with a
`manifest.json` per generation that remembers each item's package and original
path. When the system scope trashes a path in the home (Flatpak data), the
directory is named `system-<generation>`, so it never collides with the user
scope's own numbering. The engine does the moving, after every backend has
removed the package and under the apply's journal, so every package source
behaves the same and a failing apply puts the data back. Plan lists the paths
that will move on the package's removal line (`data to trash: ...`).

A package that returns, whether through `fspec rollback` or a plain re-add,
gets back the data the newest generation trashed for it, unless something
exists at the path again, in which case the trashed copy stays. A package can
opt out with `keep_data = True`.

Attribution runs during every user-scope apply, even one with nothing to
change. The user state keeps the set of entries seen under the four XDG
directories; the first apply only records it, and later ones claim entries
that are new since and named like a package (its name, a Flatpak id or its last
segment, an npm scope or the name after it, case-insensitive). Claims live in
the user state under `data` and are dropped once the package or the path is
gone. Source-known data without a Flatpak id is a small built-in table of XDG
directory names keyed by package name; `data = [...]` covers the rest.

`fspec orphans` lists entries directly under those four directories that no
package claims (declared, source-known or attributed) and no managed file
sits inside. `fspec orphans --claim <path> <pkg>` records one as the package's
data. `fspec orphans --delete <path>` moves one to the user trash.

`fspec gc --older-than 14d` (units `s`, `m`, `h`, `d`, `w`) works per scope. A
generation is kept when it is the current one or its snapshot is newer than
the cutoff. Everything else last modified at or before the cutoff goes: its
`generations/<n>.json` snapshot, its trash directory, and, in the system
scope, store paths that no kept snapshot and nothing under `current` links to,
plus their `<dir>.json` sidecars and old `generations/<n>` link directories.
It prints each deletion and the space freed. `--older-than 0s` deletes
everything the current generation does not need.

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

A key with no last-applied value that already exists on disk is a conflict, except
for `list_union` and `list_keyed`, which join the existing list. Rewriting a patched file
reserializes it, so comments in it are lost. A conflict under `on_conflict = "fail"` aborts
`apply` during prepare, before anything live changes.

Lists need a strategy: `list_union` (ensure these items, keep others),
`list_replace` (exact list) or `list_keyed(field)` (merge objects by a field).

State records the sha256 of each managed file and secret, and `plan` also reports `source changed`
when the repo `src` differs from what was written. Removal deletes a file only while it still
matches that hash (a link: while it is still our symlink); otherwise it is kept with a warning.
`secret` decrypts with `<home>/.config/fspec/age.key`. After `fspec adopt` copies a file into the
repo it rewrites the state hash, so the file stops reading as drifted.

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
| runit (Void) | `/etc/sv/fspec-<name>/run`, linked into `/etc/runit/runsvdir/default`; user services under a per-user `runsvdir ~/.local/service` |
| OpenRC | `/etc/init.d/fspec-<name>` with `supervise-daemon`; no user services |
| dinit | `/etc/dinit.d/fspec-<name>`, user services in `~/.config/dinit.d` |
| s6 | an s6-rc longrun source directory under `/etc/s6/sv`, recompiled on every change; no user services |
| launchd | plists in `/Library/LaunchDaemons` or `~/Library/LaunchAgents` |

A timer's `schedule` is a five-field cron expression or one of `@hourly`,
`@daily`, `@weekly` and `@monthly`. Timers become systemd timers, cron entries or launchd
calendar intervals. Features only systemd has (socket activation, cgroup
limits, `graphical-session.target`) go under `systemd = {...}`; on another init
`plan` fails and says which field, rather than dropping it.

systemd units are `fspec-<name>.service` (and `.timer`) under
`/etc/systemd/system` or `~/.config/systemd/user`. A bare command name in `exec`
runs through `/usr/bin/env`, since systemd does not search `PATH`. `after =
["network"]` becomes `network-online.target`, another name `fspec-<name>.service`,
and a literal `*.target` stays as written. Under `systemd = {...}`, the keys
`unit`, `service` and `install` are dicts merged into those sections and any
other key is a `[Service]` directive; an extra replaces a generated directive of
the same name, and a list repeats the directive. A timer is a oneshot service plus
a timer with `Persistent=true`. A cron schedule that restricts both day-of-month
and day-of-week fails `plan` (cron ORs them, systemd ANDs them).

launchd jobs are `fspec.<name>` plists. `exec` is split with shell rules into
`ProgramArguments`; when it holds shell syntax (pipes, redirects, `$`, globs) it
runs as `/bin/sh -c`. `after` and `description` have no launchd equivalent and
are ignored. A timer becomes one `StartCalendarInterval` entry per combination of
listed values, capped at 64 entries.

runit, OpenRC, dinit and s6 share two rules. `after = ["network"]` maps where the
init has a network service (OpenRC `need net`, dinit `depends-on = network` for
system services) and is ignored on runit and s6; a literal `*.target` fails
`plan`. Another service name orders after `fspec-<name>`. `description` is
written where the init has a place for it (OpenRC, a dinit comment) and dropped
elsewhere. `exec` is run as written by runit and s6 (`exec <exec>` at the end of
a `#!/bin/sh` script with `exec 2>&1` and `export K=V` lines before it). OpenRC
and dinit get an argument vector: shell syntax runs as `/bin/sh -c`, a bare
command name goes through `/usr/bin/env`, and dinit sets `env` through a leading
`/usr/bin/env K=V`.

runit services are `/etc/sv/fspec-<name>/run`, linked into the default runlevel
`/etc/runit/runsvdir/default`, which `/var/service` resolves to once booted. The
link cannot go through `/var/service` itself: that points into `/run`, which a
chroot (`switch --distro`) does not have.
runsvdir starts a new link within five seconds, so an add runs no `sv` command;
an update runs `sv restart` and a removal `sv down` before unlinking. runit
always restarts, so `restart = "no"` adds a `finish` running `sv down .` and
`on_failure` one that does so only on exit status 0. A dependency becomes an
`sv check ../fspec-<dep> || exit 1` line in `run`. User services are plain
directories in `~/.local/service`, scanned by `runsvdir` running as the system
service `fspec-runsvdir-<user>` (`/etc/sv/fspec-runsvdir-<user>/run`, started
with `chpst -u <user>`). The first user service creates it, so that first apply
needs the privileges to write `/etc/sv` and `/etc/runit`; the runsvdir service
is not part of the user state and stays when the last user service goes.

OpenRC services are `/etc/init.d/fspec-<name>` (`#!/sbin/openrc-run`) with
`supervisor=supervise-daemon`, `command`, `command_args` and a `depend()` block,
added with `rc-update add fspec-<name> default` and started with `rc-service`.
supervise-daemon respawns on any exit, so `on_failure` behaves as `always`;
`restart = "no"` drops the supervisor and uses `command_background`. OpenRC has
no user services, so `user = True` fails `plan`.

dinit services are `/etc/dinit.d/fspec-<name>` (or `~/.config/dinit.d/`) with
`type = process`, `command`, `restart = true | on-failure | false` and
`depends-on`. Apply runs `dinitctl [--user] enable` and `start`; an update runs
`reload` then `restart`, since dinit keeps the description it loaded. `enable`
needs the `boot` service to have a `waits-for.d` directory.

s6 services are s6-rc longrun source directories, `/etc/s6/sv/fspec-<name>/`
with `type`, `run`, an optional `finish` (exit 125 stops s6-supervise from
restarting) and `dependencies.d/fspec-<dep>`. Every fspec service is also listed
in the bundle `/etc/s6/sv/fspec`, which the host's boot bundle must include.
After the files change, fspec compiles all of `/etc/s6/sv` into
`/etc/s6/db/fspec-g<generation>-<a|r>` with `s6-rc-compile`, switches to it with
`s6-rc-update`, and starts the bundle with `s6-rc -u change fspec`. Old
databases stay in `/etc/s6/db`. fspec sets up no per-user s6 tree, so
`user = True` fails `plan`.

Timers on these four inits are cron entries with the schedule written as given.
A system timer is `/etc/cron.d/fspec-<name>` (run as root); a user timer is a
block between `# BEGIN fspec:<name>` and `# END fspec:<name>` in the user's
crontab, read with `crontab -l` and written with `crontab -`. A timer name with a
`.` fails `plan` (run-parts crons skip such files), and `%` in `exec` is
escaped. fspec assumes a cron that reads `/etc/cron.d` (cronie, vixie-cron) and
does not detect the flavour; dcron, busybox crond and snooze are out of scope.

`user` creates Linux accounts with `useradd` and adds missing groups with
`usermod -aG` (it never removes a group). On macOS it only sets the login shell
and checks the account and uid; it never creates accounts. Removing a `user`
stops managing it and leaves the account. `defaults` journals the previous value
of each key so a failed apply restores it, and removal deletes a key only while
it still holds the value fspec wrote.

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

`fspec image containerfile --host g815` prints the Containerfile: `FROM` the
base, `dnf install -y` for the `dnf(...)` packages (other package managers are
an error), the kernel arguments as `/usr/lib/bootc/kargs.d/fspec.toml`
(`kargs = [...]`), and `bootc container lint` last.

`fspec ci init` writes `.github/workflows/fspec-images.yml` next to the spec.
The workflow `fspec-images` runs on pushes to main, weekly and on demand. For
each host with an `image(...)` it renders the Containerfile, builds it with
podman and pushes `ghcr.io/<owner>/fspec-<host>:latest`. The owner comes from
the `origin` remote, or `--registry ghcr.io/<owner>`. A second job runs
`fspec update --images --registry …`, which resolves each tag to its digest
(anonymous ghcr token, then the manifest request; the packages must be public)
and opens a PR with the new `[system.<host>] image = "<repo>@sha256:…"`. Rerun
`fspec ci init` when hosts are added.

Apply runs `bootc switch <repo>@<digest>` when the locked digest differs from
the staged or booted one in `bootc status --json`, journaling `bootc rollback`
as the undo, and prints that a reboot activates it. With no lock entry, `plan`
says to run `fspec ci init`, let the workflow push, then `fspec update
--images`. Rollback is the previous deployment in the boot menu.

On classic distros (Fedora Workstation, Arch, Debian, Void) it is
`fedora(...)`, `arch(...)` and so on, driving the package manager directly with
cached pinned packages, and kernel rollback comes from btrfs snapshots.

The `packages` list of `fedora(...)`, `arch(...)`, `debian(...)` and `void(...)`
is installed by name with `dnf`, `pacman -S --needed`, `apt-get` and
`xbps-install`, and a name removed from the list is removed from the machine.
The base packages are not in the lock, so they are never pinned or cached;
pin a package that matters by declaring it as a `pkg` as well. Dropping the
whole `fedora(...)` declaration leaves its packages installed. Before any
change to the system layer, if `findmnt -no FSTYPE /` says btrfs, apply takes a
read-only snapshot `/.snapshots/fspec-<generation>`; it is taken while
preparing, before the package phases commit, so it holds the old kernel. A
failed apply leaves the snapshot behind. Other filesystems get no snapshot.
`pkg` sources for the four managers cache the locked `.rpm`, `.pkg.tar.zst`,
`.deb` or `.xbps` under `/var/cache/fspec/pkgs` and install the file (`dnf
install`, `pacman -U`, `apt-get install`, and for `.xbps` a local repository
directory built with `xbps-rindex -a` and used through `xbps-install -R`). A
rollback reinstalls the previous cached file, or the plain name when it is gone.
Xbps has no exact-version flag here, so the local repository pins by being the
only place that file lives; if the Void repos hold a newer version, xbps may
prefer it.

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
any action under those paths. The list is exactly what the host declares, with
no default. Plan fails on a file, secret or data path under an entry. An entry
that is an ancestor of the applying user's home leaves that home usable, since
the spec has to manage it; other users' homes stay refused. The spec pins your UID so ownership survives a
hop.

Between bootc distros, change `image(base = ...)` and apply. FormalSpec
rebuilds your image on the new base and runs `bootc switch`; you reboot into
the new distro with the old one still in the boot menu. Home, Flatpaks, store
packages and user services are outside the image and carry over.

`fspec switch --reset-var` (run after that apply, as root) resets `/etc` and
`/var` to the new image's defaults. The running system cannot reset its own
`/etc`, so the command only stages the reset for the next boot. It refuses
unless the machine is a bootc deployment with a staged image. It writes, under
`/var/lib/fspec/reset-var/`, a `keep` list (fspec's own state `/var/lib/fspec`,
`/opt/fspec` and `/var/opt/fspec`, the homes `/home`, `/var/home` and
`/var/roothome`, then the host's `persist` paths), a `reset.sh`, and finally a
`pending` marker. It also installs and enables `fspec-reset-var.service`, a
oneshot ordered after `local-fs.target` and before `sysinit.target` and
`systemd-tmpfiles-setup.service`, which only runs while `pending` exists. The
script deletes everything under `/var` and `/etc` that is neither in `keep` nor
a parent of something in it (it descends into parents instead of deleting
them), copies `/usr/etc` over `/etc`, and removes `pending` last, so an
interrupted run repeats on the next boot. tmpfiles then recreates the image's
`/var` defaults. Remove `pending` to cancel. The account databases
(`/etc/passwd`, `/etc/shadow`, `/etc/group`, `/etc/gshadow`, `/etc/subuid`,
`/etc/subgid`) are always kept, like the homes: `apply` recreates declared users
but not their passwords.

To or from a classic distro, `fspec switch --distro void` (fedora, arch,
debian, ubuntu or void). The host's `system` must already declare the target
distro, so edit `system =` to `void(...)` first and pass `--host` for that
host; the command refuses any other host. Nothing is built from a second
declaration. Every root lives on one disk in one of the layouts below, side by
side with the others, and boots through one shared bootloader.

### Disk layout

The default layout, the one `fspec disk init` creates and the one to use:

- A GPT disk.
- A 2 GiB EFI system partition (FAT32), mounted at `/efi` in every root as the
  UAPI Boot Loader Specification places it, and shared by every distro and by
  Windows if it is there. 2 GiB holds several distros' unified kernel images
  next to Windows' loader.
- Optionally a LUKS2 container holding the rest of the disk.
- One btrfs filesystem with the subvolumes `@<distro>` (one per root), `@home`
  (shared), `@swap` (no copy-on-write, holding `swapfile`), `@snapshots`, and
  `@fspec` mounted at `/opt/fspec`, so a switch does not download the store
  again. Store paths are keyed by the artifact's hash and the lock keys
  artifacts by platform, so every root of a host converges on the same store
  and `current`.
- systemd-boot on the ESP. Kernels go onto the ESP per distro, so roots never
  overwrite each other's kernels or bootloader config: unified kernel images
  as `/efi/EFI/Linux/<distro>-*.efi` where the distro builds them, otherwise
  `/efi/<distro>/` with a Boot Loader Specification entry in
  `/efi/loader/entries/<distro>-*.conf`. systemd-boot finds both at boot time,
  so a kernel update in a root that does not own boot never leaves a stale
  entry. Each root keeps its own `/boot` for the distro's packages, and its
  own tooling writes the ESP files: `kernel-install` with `layout=uki` on
  Fedora, Debian and Ubuntu, a `default_uki` mkinitcpio preset on Arch, and a
  kernel hook fspec installs on Void, which has no UKI tooling. The kernel
  command line comes from each root's `/etc/kernel/cmdline`.
- With LUKS, every root gets an `/etc/crypttab` line, `cryptsetup` in its
  initramfs and `rd.luks.name=` on its command line.

Two more layouts are supported:

- ZFS: a pool `rpool` with `rpool/ROOT/<distro>` boot environments,
  `rpool/home` and `rpool/fspec`, booted by ZFSBootMenu from the ESP.
  ZFSBootMenu is built for several boot environments on one pool: it reads
  each root's kernels from that root's own `/boot`, so nothing is copied to
  the ESP. Every root needs the ZFS module in its initramfs, which Debian
  (from `contrib`, built by DKMS), Ubuntu and Void provide; Arch and Fedora
  ship ZFS only from third-party repositories, so fspec refuses them on ZFS.
  Out-of-tree ZFS modules are not signed by the distro's keys, Ubuntu's being
  the exception, so Secure Boot, and with it EAC Linux v2, only works with
  Ubuntu on ZFS; `plan` says so.
- BIOS (or an MBR disk): GRUB in BIOS mode, from a `bios_grub` partition on
  GPT or the MBR gap, and a shared ext4 boot partition mounted at
  `/var/lib/fspec/boot` in every root. Each root bind-mounts its own folder
  `<distro>/` from it at `/boot`, so its packages write their kernels there,
  and GRUB itself lives in the folder of the root that owns boot. No unified
  kernel images. GRUB's menu is written when the owner runs `grub-mkconfig`,
  so every root keeps stable kernel names in its folder (Arch's own
  `vmlinuz-linux`, Debian's `link_in_boot` symlinks, and a kernel hook on
  Fedora and Void). `plan` says that Secure Boot, and so EAC, is impossible
  there.

Each distro's recipe declares what it needs: the smallest ESP it fits on,
the filesystems it boots from, whether it builds unified kernel images, and
whether bootc can install it (`--to-image` needs EFI and a btrfs root, since
bootc supports neither a ZFS root nor the shared BIOS boot partition). `fspec
plan` prints the layout it found and, when the host declares a distro other
than the running one, whether the switch fits the disk; a switch the disk
cannot satisfy is refused with the fix, usually a `fspec disk` command.

### Getting a disk onto a supported layout

- `fspec disk plan [<device>]` prints the steps that bring the running disk
  (or `<device>`) onto a supported layout, and which of them need the disk
  unmounted.
- `fspec disk migrate [<device>]` runs the steps that can be done in place:
  `btrfs-convert` from ext4, adding the shared subvolumes and moving `/home`
  and `/opt/fspec` into them, moving a root that sits at the top level into
  `@<distro>`, putting the root's kernels on the ESP, and carving a 2 GiB ESP
  (or the BIOS boot partition) out of the btrfs partition: the filesystem and
  its partition shrink, a new partition takes the freed space at the end of
  the disk, the old ESP's files are copied over, and the new ESP takes the
  old one's partition GUID and FAT serial, so firmware entries and every
  root's `fstab` keep pointing at it. The old ESP is retyped as Linux data
  and given a new GUID and serial, and stays as a copy. A partition cannot be
  grown into the space in front of a btrfs filesystem, which is why the ESP
  moves rather than grows. Encrypting an existing disk is not migrated, and
  neither is shrinking a btrfs filesystem inside LUKS; both need
  `fspec disk init`. An ext4 root converts only while unmounted, so
  `disk migrate <device>` runs from a live system, keeps the filesystem
  UUID, fixes the root's `fstab` line and rebuilds its initramfs and GRUB
  config; the rest of the steps run after booting it. After a reboot into
  `@<distro>`, `disk plan` lists what the move left behind (the old root's
  files, the copies of `/home` and `/opt/fspec` under their new mounts) as
  destructive steps.
- `fspec disk init <device> [--layout default|zfs|bios] [--luks] [--distro d
  --host h]` partitions and formats a disk from scratch, creates the shared
  subvolumes or datasets, installs the bootloader, and with `--distro`
  installs the first root the same way `switch --distro` does. It refuses a
  partition, a disk with anything mounted, and a disk under 16 GiB. ZFS
  takes no `--luks`, since it encrypts datasets itself.

Every destructive step asks for the device name to be typed back, refuses a
mounted target unless the step is built for one (`btrfs filesystem resize`
and the subvolume moves run on the mounted filesystem), and first checks that
a backup exists: a read-only snapshot under `@snapshots` taken in the last 24
hours, or `--backup <path>` naming a backup modified in the last 24 hours. A
blank disk needs neither.

### Switching

The switch reads the machine through the runner: the layout (`lsblk` and
`findmnt`), the firmware (EFI when `/sys/firmware/efi` exists), the running
root and its kernel command line. Then it:

1. Checks the target distro's recipe against the layout and stops with the
   fix if it does not fit. It refuses unless the running root owns boot when
   the layout is BIOS (see below). The first switch on a machine records the
   running root as the owner.
2. Creates the root, `@<distro>` or `rpool/ROOT/<distro>`, and mounts it under
   `/run/fspec/switch/`.
3. Bootstraps the base system from the running OS: `dnf --installroot` (needs
   `--release <n>`; the Fedora repositories are named on the command line,
   since a fresh root has none), `pacstrap -K`, `debootstrap` (`--release` is
   the suite, default `stable` on Debian and `noble` on Ubuntu) or
   `xbps-install -r` (answering its repository key prompt). The bootstrap
   includes the recipe's boot packages for the layout: UKI tooling, GRUB,
   `cryptsetup` or ZFS.
4. Writes the new root's `fstab` (`/`, `/home`, `/efi`, `/opt/fspec`, the
   swapfile, and on BIOS the shared boot partition and the `/boot` bind),
   `crypttab`, `/etc/kernel/cmdline` and the recipe's kernel tooling config,
   and copies `resolv.conf`. The command line keeps the running kernel's
   options (`console=`, `net.ifnames=0`, extra `rootflags=`) with only the
   root swapped, since the machine's serial and network setup can depend on
   them.
5. Applies the spec inside it. `--root` only logs commands, so the apply runs as
   `chroot <new root> fspec --spec <copy of the spec> apply --local --scope
   system --host <host>` with the running `fspec` and the spec directory copied
   in, the shared `/home`, `/opt/fspec` and `/efi` bound in (so a declared
   user's home lands in the shared one), and a `PATH` with the `sbin`
   directories, which Arch's lacks. The new distro's backends do the work. The
   binary must run in the new root, so it has to be static: a glibc or
   nix-store build does not run in a fresh root. systemd units are enabled
   without `--now`, which `systemctl` refuses in a chroot, and then started,
   which it ignores there. The user scope applies at first login.
6. Runs the recipe's kernel tooling in the new root once, which puts its
   kernels where the layout boots them (on ZFS it sets the boot environment's
   `org.zfsbootmenu:commandline`), and on BIOS regenerates the owner's GRUB
   menu.
7. Unmounts, also after a failure, and says to run `fspec switch --adopt-boot`
   from the new root before forgetting the old one.

Under `--root` the probes are skipped and the default layout is assumed, with
the rooted system owning boot, so the plan is logged to
`<root>/var/log/fspec-commands.log` without running anything.

### Who owns boot

Exactly one root owns boot: on the default layout the one systemd-boot boots
by default, on ZFS the pool's `bootfs`, on BIOS the one whose GRUB the boot
sector loads. On BIOS the owner is recorded with its kernel command line in
`.fspec-boot-owner` on the btrfs top level, which every root can mount. `fspec
plan`, run as root on a classic distro, prints which root owns boot.

`fspec switch --adopt-boot`, run from the new root, makes it the owner:

- Default layout: `default <distro>-*` in `/efi/loader/loader.conf`, and
  `bootctl set-default ""` so no EFI variable outranks it.
- ZFS: `zpool set bootfs=rpool/ROOT/<distro> rpool`.
- BIOS: `grub-install --target=i386-pc --boot-directory=/boot` (`grub2-install`
  on Fedora) to the disk's boot sector, and `grub-mkconfig`, whose
  `/etc/grub.d/42_fspec` lists every other root it finds in the shared boot
  partition with the kernel options that root recorded there. Then the marker.

bootc roots keep bootupd as their bootloader owner; `--adopt-boot` refuses them
for now and says so.

`fspec switch --to-image` turns a classic machine into the host's bootc image
with `podman run ... <locked image> bootc install to-existing-root
--acknowledge-destructive`. It needs an `image(...)` host with a digest in
`fspec.lock`, refuses a machine that already runs bootc, refuses a layout
bootc cannot install to, and needs `--yes` because the install reinitialises
`/boot`.

The old root stays bootable until `fspec forget <distro>` deletes it. It
refuses the running distro and the root it booted from, and stops when that
root does not exist. It is a hard refusal, not a warning, whenever the running
root does not own boot (the default entry, `bootfs` or the GRUB owner points
at another root, or nothing records an owner); the message says to run `fspec
switch --adopt-boot` from the running root first. That way the bootloader
never goes with a deleted root. It removes the root's kernels and entries from
the ESP or its folder in the shared boot partition (and regenerates GRUB's
menu on BIOS), then deletes the root with `btrfs subvolume delete --recursive`
or `zfs destroy -r`, which takes every leftover system file with it. All of
these need root; every step is a pure planning function in `fspec-switch`
that returns the command list.

Only the `system` block differs per distro. Everything from FormalSpec's own
sources, Flatpaks and files moves unchanged. Base packages are named per
distro, so the `system` block is where those names live.

## Commands

| Command | Does |
| --- | --- |
| `fspec update [pkg] [--cache]` | Resolve upstream, rewrite the lock; `--cache` records CI-built OCI digests |
| `fspec update --images` | Pin each image host's pushed digest |
| `fspec image containerfile --host h` | Print the Containerfile for a bootc host |
| `fspec build --pkg p --platform t --out d` | Build a `git(...)` package into a `.tar.zst` |
| `fspec ci init [--registry r]` | Write the image build workflow |
| `fspec ci packages` | Write the workflow that builds and pushes patched packages |
| `fspec plan [--all] [--dirty]` | Diff config against state, per host; `--all` asks each remote host over ssh |
| `fspec apply [--host h] [--all] [--dirty]` | Converge, over ssh for remote hosts |
| `fspec rollback [gen]` | Previous generation, data included |
| `fspec shell-hook <fish\|zsh\|bash>` | Shell code that applies `.fspec.toml` pins |
| `fspec gc --older-than 14d` | Drop old generations, store paths and trash |
| `fspec adopt <path>` | Pull an app's edit of a managed file into the repo |
| `fspec orphans` | Unattributed app data |
| `fspec watch` | Report drift live |
| `fspec switch --distro d [--release r]` | Install another distro beside this one |
| `fspec switch --adopt-boot` | Make the running root own boot, so the old one can be forgotten |
| `fspec switch --reset-var` | Stage a reset of `/etc` and `/var` for the next boot of a bootc host |
| `fspec switch --to-image --yes` | Convert a classic machine to the host's bootc image |
| `fspec forget <distro>` | Delete an old distro root, from the root that owns boot |
| `fspec disk plan [device]` | Steps that bring a disk onto a supported layout |
| `fspec disk migrate [device]` | Run the in-place steps of `disk plan` |
| `fspec disk init <device>` | Partition and format a disk with a supported layout |

## Remote hosts

A host is remote when its name is not this machine's short host name. It is
reached by the ssh alias in its `ssh` field (`host(name = "g815", ssh =
"kyan@g815.lan", ...)`), which defaults to the host name, so `~/.ssh/config`
decides keys, ports and jump hosts.

`fspec apply --host h` on a remote host does four things, every command going
through the `Runner`:

1. Probe: `ssh -o ConnectTimeout=5 -o BatchMode=yes h fspec facts --json`
   prints the machine's platform and distro. They must equal what the
   declaration implies, otherwise apply fails before touching anything:
   `host g815 is declared as x86_64-linux/bazzite but the machine is
   aarch64-linux/fedora`. The probe is a check, never an input to evaluation.
   A missing `fspec` on the remote fails with an install hint.
2. Refuse a dirty spec tree. Hosts that apply the same commit end up with the
   same software, so uncommitted changes under the spec directory stop the
   apply unless `--dirty` is passed.
3. Ship: `git ls-files` in the spec directory plus `fspec.lock` go through
   `tar` and ssh into `~/.cache/fspec/spec/<commit>/` on the remote. A dirty
   tree lands in `<commit>-dirty/`, wiped first.
4. Run `ssh -t h fspec --spec <dir>/spec.star apply --local --host h` with the
   terminal handed over, so sudo can prompt. `--local` stops the remote end
   from forwarding the apply again if its host name differs from the declared one.

`fspec apply --all` applies the remote hosts in spec order, then this machine,
and stops at the first failure with the hosts that converged and the ones not
attempted. `fspec plan --all` probes each host: a reachable remote host is
shipped the spec and runs `fspec plan --json --host h`, whose plan is rendered
locally; an unreachable one prints `unreachable, skipped`, since its state lives
on the host. Runs under `--root` never go remote.

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
| `fspec-files` | The `file` and `secret` backend: the four modes, format writers, the three-way patch merge, `adopt`, `watch` |
| `fspec-store` | The store backend: hash check, unpack, generations, `current`, npm and nix packages, project pins, `gc` |
| `fspec-image` | Containerfile and workflow generation, ghcr digest resolution, the bootc `image` backend |
| `fspec-sources` | Resolvers (aqua, github, npm, cargo, flatpak, brew, git, nix, dnf, pacman, apt, xbps) and `fspec update` |
| `fspec-pkgmgr` | Flatpak, Homebrew and the distro package backends (dnf, pacman, apt, xbps), and the base packages of a classic distro |
| `fspec-os` | Services and timers per init (systemd, launchd, runit, OpenRC, dinit, s6, cron), users, macOS `defaults` |
| `fspec-remote` | Remote hosts: ssh and tar command lines, the spec shipment, the probe check, `--all` ordering |
| `fspec-switch` | `fspec switch` and `forget`: btrfs and bootloader detection, the boot owner record, the command plans per distro and for `--adopt-boot`, the `--reset-var` staging |
| `fspec-build` | Source builds: recipe detection, the `.tar.zst` packer, the `ci packages` workflow, `update --cache` |
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

All eight steps are implemented and tested against temp roots with recorded
commands. Steps 1 to 6 have also run live on macOS and on the four classic
distros; bootc and `fspec switch` have not.

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
