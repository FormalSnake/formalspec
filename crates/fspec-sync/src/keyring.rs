//! Secret Service items through `secret-tool` (libsecret). Secrets stay in memory: they come
//! back on stdout and go in on stdin, never through a file.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use fspec_core::resource::KeyringItem;
use fspec_core::runner::{Cmd, Runner};
use serde::{Deserialize, Serialize};

/// An item as `secret-tool search --all` prints it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Found {
    pub label: String,
    /// Missing when the item's collection is locked.
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub modified: String,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub attrs: BTreeMap<String, String>,
}

impl Found {
    /// The item's key in a manifest: its schema and every attribute, so two hosts name the same
    /// item the same way.
    pub fn key(&self) -> String {
        let attrs: Vec<String> = self.attrs.iter().map(|(k, v)| format!("{k}={v}")).collect();
        format!("{}|{}", self.schema.as_deref().unwrap_or(""), attrs.join(","))
    }

    /// `modified = 2026-10-21 15:24:50` as a number that orders the same way.
    pub fn time(&self) -> i64 {
        self.modified.chars().filter(char::is_ascii_digit).collect::<String>().parse().unwrap_or(0)
    }
}

/// Attribute names a replaced item keeps under, so no app's lookup matches it.
const PREV: &str = "fspec-prev:";
const PREV_ITEM: &str = "fspec-prev-item";

pub fn parse(out: &str) -> Vec<Found> {
    let mut items = Vec::new();
    let mut cur: Option<Found> = None;
    for line in out.lines() {
        if line.starts_with('[') && line.ends_with(']') {
            items.extend(cur.take());
            cur = Some(Found::default());
            continue;
        }
        let (Some(item), Some((k, v))) =
            (cur.as_mut(), line.split_once(" = ").or_else(|| line.strip_suffix(" =").map(|k| (k, ""))))
        else {
            continue;
        };
        match k {
            "label" => item.label = v.to_string(),
            "secret" => item.secret = Some(v.to_string()),
            "modified" => item.modified = v.to_string(),
            "schema" => item.schema = Some(v.to_string()),
            _ => {
                if let Some(name) = k.strip_prefix("attribute.") {
                    item.attrs.insert(name.to_string(), v.to_string());
                }
            }
        }
    }
    items.extend(cur);
    items
}

fn search(runner: &dyn Runner, query: &[(String, String)]) -> Result<Vec<Found>> {
    let mut cmd = Cmd::new("secret-tool").args(["search", "--all"]);
    for (k, v) in query {
        cmd = cmd.arg(k).arg(v);
    }
    let out = runner.run(&cmd)?;
    // secret-tool exits 1 with nothing on stderr when no item matches.
    if out.status != 0 {
        if out.stderr.trim().is_empty() {
            return Ok(vec![]);
        }
        bail!("`secret-tool search` exited {}: {}", out.status, out.stderr.trim());
    }
    Ok(parse(&out.stdout))
}

/// The items `k` matches.
pub fn find(runner: &dyn Runner, k: &KeyringItem) -> Result<Vec<Found>> {
    let mut query: Vec<(String, String)> =
        k.attrs.iter().filter(|(_, v)| !v.ends_with('*')).map(|(a, v)| (a.clone(), v.clone())).collect();
    if let Some(schema) = &k.schema {
        query.push(("xdg:schema".into(), schema.clone()));
    }
    if query.is_empty() {
        bail!("a keyring item needs a schema or one attribute without `*` to search by");
    }
    let found = search(runner, &query)?;
    Ok(found
        .into_iter()
        .filter(|f| k.schema.is_none() || f.schema == k.schema)
        .filter(|f| k.label.as_ref().is_none_or(|l| *l == f.label))
        .filter(|f| {
            k.attrs.iter().all(|(a, v)| match v.strip_suffix('*') {
                Some(prefix) => f.attrs.get(a).is_some_and(|have| have.starts_with(prefix)),
                None => f.attrs.get(a) == Some(v),
            })
        })
        .collect())
}

/// The item with exactly this schema and these attributes.
pub fn exact(runner: &dyn Runner, schema: Option<&str>, attrs: &BTreeMap<String, String>) -> Result<Option<Found>> {
    let mut query: Vec<(String, String)> = attrs.iter().map(|(a, v)| (a.clone(), v.clone())).collect();
    if let Some(s) = schema {
        query.push(("xdg:schema".into(), s.into()));
    }
    Ok(search(runner, &query)?.into_iter().find(|f| f.attrs == *attrs && f.schema.as_deref() == schema))
}

pub fn store(runner: &dyn Runner, f: &Found) -> Result<()> {
    let Some(secret) = &f.secret else { bail!("keyring item {} has no secret to store", f.label) };
    let mut cmd = Cmd::new("secret-tool").arg("store").arg(format!("--label={}", f.label));
    for (k, v) in &f.attrs {
        cmd = cmd.arg(k).arg(v);
    }
    if let Some(s) = &f.schema {
        cmd = cmd.args(["xdg:schema", s]);
    }
    runner.check(&cmd.stdin(secret.clone()))?;
    Ok(())
}

/// Keeps `f` as the previous version of a keyring item of sync item `item`.
pub fn store_prev(runner: &dyn Runner, item: &str, f: &Found) -> Result<()> {
    let mut attrs: BTreeMap<String, String> = f.attrs.iter().map(|(k, v)| (format!("{PREV}{k}"), v.clone())).collect();
    attrs.insert(PREV_ITEM.into(), item.into());
    if let Some(s) = &f.schema {
        attrs.insert(format!("{PREV}xdg:schema"), s.clone());
    }
    store(runner, &Found { attrs, schema: None, ..f.clone() })
}

/// The previous versions kept for `item`, as the items they replaced.
pub fn prevs(runner: &dyn Runner, item: &str) -> Result<Vec<Found>> {
    let found = search(runner, &[(PREV_ITEM.into(), item.into())])?;
    Ok(found
        .into_iter()
        .map(|mut f| {
            let attrs = std::mem::take(&mut f.attrs);
            f.schema = attrs.get(&format!("{PREV}xdg:schema")).cloned();
            f.attrs = attrs
                .into_iter()
                .filter_map(|(k, v)| k.strip_prefix(PREV).filter(|k| *k != "xdg:schema").map(|k| (k.to_string(), v)))
                .collect();
            f
        })
        .collect())
}

pub fn clear_prevs(runner: &dyn Runner, item: &str) -> Result<()> {
    let out = runner.run(&Cmd::new("secret-tool").args(["clear", PREV_ITEM, item]))?;
    if out.status != 0 && !out.stderr.trim().is_empty() {
        bail!("`secret-tool clear` exited {}: {}", out.status, out.stderr.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUT: &str = "[/1]\nlabel = Password for 'FormalSnake' on 'gh:github.com'\nsecret = tok\ncreated = 2026-06-16 17:04:11\nmodified = 2026-10-21 15:24:50\nschema = org.freedesktop.Secret.Generic\nattribute.service = gh:github.com\nattribute.username = FormalSnake\n[/20]\nlabel = Password for '' on 'gh:github.com'\nsecret = other\nmodified = 2026-10-21 15:24:50\nschema = org.freedesktop.Secret.Generic\nattribute.service = gh:github.com\nattribute.username = \n";

    #[test]
    fn parses_secret_tool_search() {
        let items = parse(OUT);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].secret.as_deref(), Some("tok"));
        assert_eq!(items[0].time(), 20261021152450);
        assert_eq!(items[1].attrs["username"], "");
        assert_eq!(items[0].key(), "org.freedesktop.Secret.Generic|service=gh:github.com,username=FormalSnake");
    }
}
