# CLAUDE.md

> fspec is basically nix if it were faster, not centralized, and compatible with any system and its package manager.

Judge every design choice, engine feature and spec pattern against that line. A change that
pulls fspec back toward how nix works for nix's own reasons is the wrong change.

- **Faster.** No full-world evaluation and no rebuilds of things a binary already exists for.
  Distro packages and upstream binaries come first; a source build is the last resort.
- **Not centralized.** There is no single nixpkgs. Any repo can ship its own `.star`, and
  recipes live with the user's spec or with the upstream project, never in a central tree.
- **Any system.** fspec runs on top of the host OS and drives its native package manager
  (zypper, pacman, dnf, apt, brew) instead of replacing it.

`docs/design.md` is the design. `docs/from-nix.md` maps nix constructs to their fspec shape;
add each mapping you settle there, and drop nix-isms that only exist to work around nix.
