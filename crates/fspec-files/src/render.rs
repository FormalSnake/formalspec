//! Serializing spec data into the file formats `file(content = ...)` supports, and parsing the
//! ones `patch` mode edits.

use anyhow::{Context, Result, bail};
use fspec_core::resource::{File, Format};
use serde_json::Value;

/// The format a path's extension implies.
pub fn infer(path: &str) -> Option<Format> {
    let ext = path.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "json" => Format::Json,
        "toml" => Format::Toml,
        "yaml" | "yml" => Format::Yaml,
        "ini" => Format::Ini,
        "plist" => Format::Plist,
        "lua" => Format::Lua,
        _ => return None,
    })
}

pub fn format_of(f: &File) -> Option<Format> {
    f.format.or_else(|| infer(&f.path))
}

pub fn serialize(value: &Value, format: Format) -> Result<String> {
    match format {
        Format::Text => bail!("`format = \"text\"` only takes a string content"),
        Format::Json => Ok(serde_json::to_string_pretty(value)? + "\n"),
        Format::Toml => toml::to_string_pretty(&to_toml(value)?).context("serializing toml"),
        Format::Yaml => serde_yaml_ng::to_string(&to_yaml(value)).context("serializing yaml"),
        Format::Ini => ini(value),
        Format::Plist => {
            let mut out = Vec::new();
            plist::to_writer_xml(&mut out, &to_plist(value)?).context("serializing plist")?;
            Ok(String::from_utf8(out)?)
        }
        Format::Lua => Ok(format!("return {}\n", lua(value, 0))),
    }
}

/// Parse a file `patch` mode edits. An empty file is an empty table.
pub fn parse(text: &str, format: Format) -> Result<Value> {
    if text.trim().is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    let value: Value = match format {
        Format::Json => serde_json::from_str(text)?,
        Format::Toml => toml::from_str(text)?,
        Format::Yaml => serde_yaml_ng::from_str(text)?,
        other => bail!("patch mode cannot read {other:?} files, only json, toml and yaml"),
    };
    if !value.is_object() {
        bail!("the top level must be a table");
    }
    Ok(value)
}

// serde_json can be built with `arbitrary_precision` by another crate in the workspace, which
// makes its numbers serialize as maps in other formats. These conversions avoid that.

fn to_toml(v: &Value) -> Result<toml::Value> {
    Ok(match v {
        Value::Null => bail!("toml has no null"),
        Value::Bool(b) => toml::Value::Boolean(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => toml::Value::Integer(i),
            None => toml::Value::Float(n.as_f64().context("number out of range")?),
        },
        Value::String(s) => toml::Value::String(s.clone()),
        Value::Array(a) => toml::Value::Array(a.iter().map(to_toml).collect::<Result<_>>()?),
        Value::Object(m) => {
            toml::Value::Table(m.iter().map(|(k, v)| Ok((k.clone(), to_toml(v)?))).collect::<Result<_>>()?)
        }
    })
}

fn to_yaml(v: &Value) -> serde_yaml_ng::Value {
    use serde_yaml_ng::Value as Y;
    match v {
        Value::Null => Y::Null,
        Value::Bool(b) => Y::Bool(*b),
        Value::Number(n) => Y::Number(match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => i.into(),
            (None, Some(u)) => u.into(),
            _ => n.as_f64().unwrap_or_default().into(),
        }),
        Value::String(s) => Y::String(s.clone()),
        Value::Array(a) => Y::Sequence(a.iter().map(to_yaml).collect()),
        Value::Object(m) => Y::Mapping(m.iter().map(|(k, v)| (Y::String(k.clone()), to_yaml(v))).collect()),
    }
}

fn to_plist(v: &Value) -> Result<plist::Value> {
    Ok(match v {
        Value::Null => bail!("plist has no null"),
        Value::Bool(b) => plist::Value::Boolean(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => plist::Value::Integer(i.into()),
            None => plist::Value::Real(n.as_f64().context("number out of range")?),
        },
        Value::String(s) => plist::Value::String(s.clone()),
        Value::Array(a) => plist::Value::Array(a.iter().map(to_plist).collect::<Result<_>>()?),
        Value::Object(m) => plist::Value::Dictionary(
            m.iter().map(|(k, v)| Ok((k.clone(), to_plist(v)?))).collect::<Result<plist::Dictionary>>()?,
        ),
    })
}

fn ini(value: &Value) -> Result<String> {
    let Value::Object(top) = value else { bail!("ini content must be a table") };
    let mut head = String::new();
    let mut sections = String::new();
    for (k, v) in top {
        match v {
            Value::Object(section) => {
                sections.push_str(&format!("\n[{k}]\n"));
                for (k, v) in section {
                    sections.push_str(&format!("{k} = {}\n", ini_scalar(k, v)?));
                }
            }
            v => head.push_str(&format!("{k} = {}\n", ini_scalar(k, v)?)),
        }
    }
    Ok(format!("{head}{sections}").trim_start_matches('\n').to_string())
}

fn ini_scalar(key: &str, v: &Value) -> Result<String> {
    match v {
        Value::String(s) => Ok(s.clone()),
        Value::Bool(_) | Value::Number(_) => Ok(v.to_string()),
        _ => bail!("ini key `{key}` must be a string, number or bool, with at most one level of sections"),
    }
}

const LUA_KEYWORDS: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in", "local", "nil",
    "not", "or", "repeat", "return", "then", "true", "until", "while",
];

fn lua(v: &Value, depth: usize) -> String {
    let pad = "  ".repeat(depth + 1);
    let close = "  ".repeat(depth);
    match v {
        Value::Null => "nil".into(),
        Value::Bool(_) | Value::Number(_) => v.to_string(),
        Value::String(s) => lua_string(s),
        Value::Array(items) if items.is_empty() => "{}".into(),
        Value::Array(items) => {
            let body: String = items.iter().map(|i| format!("{pad}{},\n", lua(i, depth + 1))).collect();
            format!("{{\n{body}{close}}}")
        }
        Value::Object(map) if map.is_empty() => "{}".into(),
        Value::Object(map) => {
            let body: String = map
                .iter()
                .map(|(k, v)| {
                    let bare = k.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                        && !LUA_KEYWORDS.contains(&k.as_str());
                    let key = if bare { k.clone() } else { format!("[{}]", lua_string(k)) };
                    format!("{pad}{key} = {},\n", lua(v, depth + 1))
                })
                .collect();
            format!("{{\n{body}{close}}}")
        }
    }
}

fn lua_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
