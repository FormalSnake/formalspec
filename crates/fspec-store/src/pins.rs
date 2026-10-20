//! Project pins: a `.fspec.toml` with `[tools] ripgrep = "14.1.0"` puts that store version's
//! executables first on PATH inside the directory, through a hook the shell runs on `cd`.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fspec_core::Paths;
use serde::Deserialize;

use crate::Sidecar;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shell {
    Fish,
    Zsh,
    Bash,
}

#[derive(Deserialize)]
struct ProjectFile {
    #[serde(default)]
    tools: BTreeMap<String, String>,
}

fn find_project(dir: &Path) -> Option<PathBuf> {
    dir.ancestors().map(|d| d.join(".fspec.toml")).find(|f| f.is_file())
}

/// The bin directories to put on PATH for `dir`, one per pinned tool found in the store.
/// Each is a small directory of links under the user's cache, since the store itself is
/// shared and a tool's tree holds more than its executables.
pub fn pin_dirs(paths: &Paths, dir: &Path) -> Result<Vec<PathBuf>> {
    let Some(file) = find_project(dir) else { return Ok(vec![]) };
    let text = fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    let project: ProjectFile = toml::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
    let store = paths.fspec_root().join("store");
    let mut found = Vec::new();
    for entry in fs::read_dir(&store).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json")
            && let Ok(text) = fs::read_to_string(&path)
            && let Ok(meta) = serde_json::from_str::<Sidecar>(&text)
        {
            found.push((path, meta));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));

    let mut dirs = Vec::new();
    for (tool, version) in &project.tools {
        let Some((sidecar, meta)) = found.iter().find(|(_, m)| &m.name == tool && &m.version == version) else {
            eprintln!("fspec: {tool} {version} from {} is not in the store", file.display());
            continue;
        };
        let dirname = sidecar.file_stem().context("sidecar has a name")?;
        let bin = paths.home.join(".cache/fspec/pins").join(dirname).join("bin");
        if !bin.exists() {
            fs::create_dir_all(&bin)?;
            for (name, rel) in &meta.bins {
                symlink(store.join(dirname).join(rel), bin.join(name))?;
            }
        }
        dirs.push(bin);
    }
    Ok(dirs)
}

/// Shell code that keeps PATH in step with the nearest `.fspec.toml`.
pub fn shell_hook(shell: Shell) -> &'static str {
    match shell {
        Shell::Fish => FISH,
        Shell::Zsh => ZSH,
        Shell::Bash => BASH,
    }
}

const FISH: &str = r#"function __fspec_env --on-variable PWD
    for d in $__fspec_pins
        set -l i (contains -i -- $d $PATH)
        and set -e PATH[$i]
    end
    set -g __fspec_pins (command fspec env --dir $PWD)
    set -gx PATH $__fspec_pins $PATH
end
__fspec_env
"#;

const ZSH: &str = r#"__fspec_env() {
  local d out
  for d in "${__fspec_pins[@]}"; do path=("${(@)path:#$d}"); done
  out=$(command fspec env --dir "$PWD")
  __fspec_pins=()
  [[ -n $out ]] && __fspec_pins=("${(@f)out}")
  path=("${__fspec_pins[@]}" "${path[@]}")
}
autoload -Uz add-zsh-hook
add-zsh-hook chpwd __fspec_env
__fspec_env
"#;

const BASH: &str = r#"__fspec_env() {
  [ "$PWD" = "$__fspec_pwd" ] && return
  __fspec_pwd=$PWD
  local d
  for d in "${__fspec_pins[@]}"; do PATH=":$PATH:"; PATH="${PATH//:$d:/:}"; PATH="${PATH#:}"; PATH="${PATH%:}"; done
  __fspec_pins=()
  while IFS= read -r d; do [ -n "$d" ] && __fspec_pins+=("$d"); done < <(command fspec env --dir "$PWD")
  for d in "${__fspec_pins[@]}"; do PATH="$d:$PATH"; done
}
PROMPT_COMMAND="__fspec_env${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
"#;
