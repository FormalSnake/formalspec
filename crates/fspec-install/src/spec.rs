//! The spec repo the installer writes into the new system.

use crate::model::{Draft, SystemChoice};

/// `spec.star` for `draft`, with the parts not chosen yet left out.
pub fn preview(d: &Draft) -> String {
    let mut out = String::from("# Written by the FormalSpec installer. Edit it, then run `fspec apply`.\n");
    let fresh = match &d.system {
        Some(SystemChoice::Fresh { distro, preset, desktop }) => Some((*distro, preset.id(), desktop.map(|d| d.id()))),
        _ => None,
    };
    if let Some((_, preset, _)) = fresh {
        out.push_str(&format!("load(\"//modules/{preset}.star\", \"{preset}\")\n"));
    }
    out.push_str("\nhosts = [\n    host(\n");
    let name = d.account.as_ref().map_or("…", |a| a.hostname.as_str());
    out.push_str(&format!("        name = \"{name}\",\n        platform = \"x86_64-linux\",\n"));
    if let Some(l) = &d.locale {
        out.push_str(&format!(
            "        locale = \"{}\",\n        keymap = \"{}\",\n        timezone = \"{}\",\n",
            l.locale, l.keymap, l.timezone
        ));
    }
    if let Some((distro, preset, desktop)) = fresh {
        let args = desktop.map(|d| format!(", desktop = \"{d}\"")).unwrap_or_default();
        out.push_str(&format!("        modules = [{preset}(\"{distro}\"{args})],\n"));
    }
    if let Some(a) = &d.account {
        out.push_str(&format!("        users = [user(\"{}\", uid = 1000, groups = [\"wheel\"])],\n", a.name));
    }
    out.push_str("        protect = [\"/home\"],\n    ),\n]\n");
    out
}
