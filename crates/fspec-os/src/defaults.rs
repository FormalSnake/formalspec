//! macOS `defaults` keys.
//!
//! Scalars are written with their type flag (`-bool`, `-int`, `-float`, `-string`). Arrays and
//! dicts use `-array` and `-dict` with every element given as an XML plist fragment, so element
//! types survive; the `defaults` command otherwise reads them all as strings.

use std::path::Path;

use anyhow::{Result, bail};
use fspec_core::Resource;
use fspec_core::apply::{Backend, Cx, Op, Phase, Records};
use fspec_core::facts::Os;
use fspec_core::journal::{Journal, Undo};
use fspec_core::resource::Defaults;
use fspec_core::runner::{Cmd, Runner};
use fspec_core::state::Entry;
use plist::{Value, XmlWriteOptions};
use serde_json::Map;

pub struct DefaultsBackend;

impl Backend for DefaultsBackend {
    fn name(&self) -> &'static str {
        "defaults"
    }

    fn handles(&self, cx: &Cx, r: &Resource) -> bool {
        cx.facts.os == Os::Darwin && matches!(r, Resource::Defaults(_))
    }

    fn phase(&self) -> Phase {
        Phase::Defaults
    }

    fn check(&self, _cx: &Cx, r: &Resource) -> Result<()> {
        let Resource::Defaults(d) = r else { return Ok(()) };
        if d.domain.is_empty() || d.domain.starts_with(['-', '/', '~', '.']) {
            bail!("defaults domain `{}` must be a bundle id or NSGlobalDomain", d.domain);
        }
        if d.key.is_empty() || d.key.starts_with('-') {
            bail!("defaults:{} {} has an invalid key", d.domain, d.key);
        }
        write_args(&to_plist(&d.value)?).map(drop)
    }

    fn drift(&self, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
        let Resource::Defaults(d) = &entry.resource else { return Ok(vec![]) };
        Ok(match read(cx.runner, &d.domain, &d.key)? {
            Live::Unreadable => vec![],
            Live::Absent => vec!["key is missing".into()],
            Live::Value(v) if v != to_plist(&d.value)? => vec![format!("value is {}", brief(&v))],
            Live::Value(_) => vec![],
        })
    }

    fn commit(&self, cx: &Cx, ops: &[Op], _staging: &Path, journal: &mut Journal) -> Result<Records> {
        let mut records = Records::new();
        for op in ops {
            let Resource::Defaults(d) = op.resource() else { unreachable!("defaults claims defaults") };
            let want = to_plist(&d.value)?;
            let live = read(cx.runner, &d.domain, &d.key)?;
            if op.is_removal() {
                match live {
                    Live::Value(v) if v == want => {
                        journal.record(Undo::Run { cmd: write(d, &v)? })?;
                        cx.runner.check(&delete(d))?;
                    }
                    Live::Absent => {}
                    _ => println!("defaults:{} {} was changed outside fspec; left in place", d.domain, d.key),
                }
                continue;
            }
            if !matches!(&live, Live::Value(v) if *v == want) {
                let undo = match &live {
                    Live::Value(old) => write(d, old)?,
                    _ => delete(d),
                };
                journal.record(Undo::Run { cmd: undo })?;
                cx.runner.check(&write(d, &want)?)?;
            }
            records.insert(op.key.clone(), Map::new());
        }
        Ok(records)
    }
}

enum Live {
    /// The command printed nothing usable (a dry run, or no preferences for the domain yet).
    Unreadable,
    Absent,
    Value(Value),
}

fn read(runner: &dyn Runner, domain: &str, key: &str) -> Result<Live> {
    let out = runner.run(&Cmd::new("defaults").args(["export", domain, "-"]))?;
    if out.status != 0 || out.stdout.trim().is_empty() {
        return Ok(Live::Unreadable);
    }
    let Some(dict) = Value::from_reader_xml(out.stdout.as_bytes())?.into_dictionary() else {
        return Ok(Live::Unreadable);
    };
    Ok(match dict.get(key) {
        Some(v) => Live::Value(v.clone()),
        None => Live::Absent,
    })
}

fn write(d: &Defaults, v: &Value) -> Result<Cmd> {
    Ok(Cmd::new("defaults").args(["write", &d.domain, &d.key]).args(write_args(v)?))
}

fn delete(d: &Defaults) -> Cmd {
    Cmd::new("defaults").args(["delete", &d.domain, &d.key])
}

fn to_plist(v: &serde_json::Value) -> Result<Value> {
    use serde_json::Value as J;
    Ok(match v {
        J::Null => bail!("a defaults value cannot be null; remove the line to delete the key"),
        J::Bool(b) => Value::Boolean(*b),
        J::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => Value::Integer(i.into()),
            (_, Some(u), _) => Value::Integer(u.into()),
            (_, _, Some(f)) => Value::Real(f),
            _ => bail!("number {n} cannot be written to defaults"),
        },
        J::String(s) => Value::String(s.clone()),
        J::Array(items) => Value::Array(items.iter().map(to_plist).collect::<Result<_>>()?),
        J::Object(map) => {
            Value::Dictionary(map.iter().map(|(k, v)| Ok((k.clone(), to_plist(v)?))).collect::<Result<_>>()?)
        }
    })
}

/// Everything after `defaults write <domain> <key>`.
fn write_args(v: &Value) -> Result<Vec<String>> {
    let s = |x: &str| x.to_string();
    Ok(match v {
        Value::Boolean(b) => vec![s("-bool"), b.to_string()],
        Value::Integer(i) => vec![s("-int"), i.to_string()],
        Value::Real(f) => vec![s("-float"), f.to_string()],
        Value::String(x) => vec![s("-string"), x.clone()],
        Value::Data(bytes) => vec![s("-data"), bytes.iter().map(|b| format!("{b:02x}")).collect()],
        Value::Array(items) => {
            let mut args = vec![s("-array")];
            for item in items {
                args.push(fragment(item)?);
            }
            args
        }
        Value::Dictionary(map) => {
            let mut args = vec![s("-dict")];
            for (k, item) in map {
                args.push(k.clone());
                args.push(fragment(item)?);
            }
            args
        }
        other => bail!("defaults cannot write {} values", brief(other)),
    })
}

/// One value as XML without the document wrapper, on a single line.
fn fragment(v: &Value) -> Result<String> {
    let mut buf = Vec::new();
    v.to_writer_xml_with_options(&mut buf, &XmlWriteOptions::default().root_element(false).indent(b' ', 0))?;
    Ok(String::from_utf8(buf)?.lines().map(str::trim).collect())
}

fn brief(v: &Value) -> String {
    match v {
        Value::Boolean(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Real(f) => f.to_string(),
        Value::String(s) => format!("{s:?}"),
        Value::Array(_) => "an array".into(),
        Value::Dictionary(_) => "a dict".into(),
        Value::Data(_) => "data".into(),
        Value::Date(_) => "a date".into(),
        _ => "an unsupported type".into(),
    }
}

#[cfg(test)]
mod tests;
