//! Three-way merge for `patch` mode, on parsed documents. The record keeps the value last
//! written per owned key (like `kubectl apply`'s last-applied annotation).

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use fspec_core::resource::{ListStrategy, OnConflict};
use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Scalar,
    Union,
    Replace,
    Keyed(String),
}

/// An owned key's desired state. `value` is the plain value, or the item array for lists.
#[derive(Clone, Debug, PartialEq)]
pub struct Want {
    pub kind: Kind,
    pub value: Value,
}

impl Want {
    pub fn parse(key: &str, v: &Value) -> Result<Want> {
        let is_strategy = v.get("kind").and_then(Value::as_str).is_some_and(|k| k.starts_with("list_"));
        if !is_strategy {
            return Ok(Want { kind: Kind::Scalar, value: v.clone() });
        }
        let strategy: ListStrategy = serde_json::from_value(v.clone()).with_context(|| format!("owned key `{key}`"))?;
        Ok(match strategy {
            ListStrategy::ListUnion { items } => Want { kind: Kind::Union, value: Value::Array(items) },
            ListStrategy::ListReplace { items } => Want { kind: Kind::Replace, value: Value::Array(items) },
            ListStrategy::ListKeyed { field, items } => {
                if let Some(bad) = items.iter().find(|i| i.get(&field).is_none()) {
                    bail!("owned key `{key}`: list_keyed item {bad} has no `{field}` field");
                }
                Want { kind: Kind::Keyed(field), value: Value::Array(items) }
            }
        })
    }

    /// The kind of a key as last declared; a key that no longer parses is treated as a plain value.
    fn kind_of(owns: &BTreeMap<String, Value>, key: &str) -> Kind {
        owns.get(key).and_then(|v| Want::parse(key, v).ok()).map(|w| w.kind).unwrap_or(Kind::Scalar)
    }
}

fn items(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}

fn key_of<'a>(field: &str, item: &'a Value) -> Option<&'a Value> {
    item.get(field)
}

/// Whether `disk` still carries everything `applied` put there.
fn holds(kind: &Kind, disk: Option<&Value>, applied: &Value) -> bool {
    let Some(disk) = disk else { return false };
    match kind {
        Kind::Scalar | Kind::Replace => disk == applied,
        Kind::Union => disk.is_array() && items(applied).iter().all(|i| items(disk).contains(i)),
        Kind::Keyed(field) => {
            disk.is_array()
                && items(applied)
                    .iter()
                    .all(|i| items(disk).iter().any(|d| key_of(field, d) == key_of(field, i) && d == i))
        }
    }
}

/// The value the key should hold after applying `want` over `disk`, given what was applied last.
fn merged(want: &Want, disk: Option<&Value>, last: Option<&Value>) -> Value {
    let mut base: Vec<Value> = disk.map(|d| items(d).to_vec()).unwrap_or_default();
    let last_items = last.map(items).unwrap_or(&[]);
    let wanted = items(&want.value);
    match &want.kind {
        Kind::Scalar | Kind::Replace => want.value.clone(),
        Kind::Union => {
            base.retain(|b| !(last_items.contains(b) && !wanted.contains(b)));
            for w in wanted {
                if !base.contains(w) {
                    base.push(w.clone());
                }
            }
            Value::Array(base)
        }
        Kind::Keyed(field) => {
            base.retain(|b| {
                let dropped =
                    last_items.iter().any(|l| l == b) && !wanted.iter().any(|w| key_of(field, w) == key_of(field, b));
                !dropped
            });
            for w in wanted {
                match base.iter_mut().find(|b| key_of(field, b) == key_of(field, w)) {
                    Some(slot) => *slot = w.clone(),
                    None => base.push(w.clone()),
                }
            }
            Value::Array(base)
        }
    }
}

enum Retract {
    Remove,
    Set(Value),
    Keep,
}

/// What dropping a key from `owns` does: only what still holds the last-applied value goes.
fn retract(kind: &Kind, disk: Option<&Value>, applied: &Value) -> Retract {
    let Some(disk) = disk else { return Retract::Keep };
    match kind {
        Kind::Scalar | Kind::Replace if disk == applied => Retract::Remove,
        Kind::Scalar | Kind::Replace => Retract::Keep,
        Kind::Union | Kind::Keyed(_) if !disk.is_array() => Retract::Keep,
        Kind::Union | Kind::Keyed(_) => {
            let left: Vec<Value> = items(disk).iter().filter(|d| !items(applied).contains(d)).cloned().collect();
            Retract::Set(Value::Array(left))
        }
    }
}

pub fn get<'a>(doc: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.').try_fold(doc, |v, part| v.get(part))
}

pub fn set(doc: &mut Value, key: &str, value: Value) {
    let mut cur = doc;
    for part in key.split('.') {
        if !cur.is_object() {
            *cur = Value::Object(Map::new());
        }
        cur = cur.as_object_mut().expect("just made an object").entry(part).or_insert(Value::Null);
    }
    *cur = value;
}

pub fn remove(doc: &mut Value, key: &str) {
    let (parents, last) = key.rsplit_once('.').unwrap_or(("", key));
    let mut cur = Some(doc);
    if !parents.is_empty() {
        for part in parents.split('.') {
            cur = cur.and_then(|v| v.get_mut(part));
        }
    }
    if let Some(Value::Object(map)) = cur {
        map.shift_remove(last);
    }
}

/// What the state keeps for one patch file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Record {
    /// Last-applied value per owned key.
    pub keys: BTreeMap<String, Value>,
    /// Keys left to the app by `on_conflict = "theirs"`, with the desired value that was declined.
    /// Managing resumes when the desired value changes.
    pub released: BTreeMap<String, Value>,
}

impl Record {
    pub fn from_map(m: &Map<String, Value>) -> Record {
        let table = |name: &str| -> BTreeMap<String, Value> {
            m.get(name)
                .and_then(Value::as_object)
                .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default()
        };
        Record { keys: table("keys"), released: table("released") }
    }

    pub fn to_map(&self) -> Map<String, Value> {
        let table =
            |t: &BTreeMap<String, Value>| Value::Object(t.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
        let mut m = Map::new();
        m.insert("keys".into(), table(&self.keys));
        if !self.released.is_empty() {
            m.insert("released".into(), table(&self.released));
        }
        m
    }
}

pub struct Merged {
    pub doc: Value,
    pub changed: bool,
    pub record: Record,
    /// Keys that differ from the last-applied value while `on_conflict = "fail"`.
    pub conflicts: Vec<String>,
}

/// Merge `owns` into `doc`. `old_owns` is the declaration the record was made from, which says
/// how to retract a key that is no longer owned.
pub fn merge(
    doc: &Value,
    owns: &BTreeMap<String, Value>,
    old_owns: &BTreeMap<String, Value>,
    record: &Record,
    policy: OnConflict,
) -> Result<Merged> {
    let mut out = doc.clone();
    let mut next = Record::default();
    let mut conflicts = Vec::new();

    for (key, declared) in owns {
        let want = Want::parse(key, declared)?;
        let disk = get(doc, key);
        let last = record.keys.get(key);

        if record.released.get(key) == Some(&want.value) {
            if let Some(l) = last {
                next.keys.insert(key.clone(), l.clone());
            }
            next.released.insert(key.clone(), want.value);
            continue;
        }

        let result = merged(&want, disk, last);
        if Some(&result) == disk {
            next.keys.insert(key.clone(), want.value);
            continue;
        }
        let untouched = match last {
            Some(l) => holds(&want.kind, disk, l),
            // Nothing applied yet: only a list that merges additively can join what is there.
            None => disk.is_none() || matches!(want.kind, Kind::Union | Kind::Keyed(_)),
        };
        if untouched {
            set(&mut out, key, result);
            next.keys.insert(key.clone(), want.value);
            continue;
        }
        match policy {
            OnConflict::Ours => {
                set(&mut out, key, result);
                next.keys.insert(key.clone(), want.value);
            }
            OnConflict::Theirs => {
                if let Some(l) = last {
                    next.keys.insert(key.clone(), l.clone());
                }
                next.released.insert(key.clone(), want.value);
            }
            OnConflict::Fail => conflicts.push(key.clone()),
        }
    }

    for (key, last) in &record.keys {
        if owns.contains_key(key) {
            continue;
        }
        match retract(&Want::kind_of(old_owns, key), get(doc, key), last) {
            Retract::Remove => remove(&mut out, key),
            Retract::Set(v) => set(&mut out, key, v),
            Retract::Keep => {}
        }
    }

    let changed = out != *doc;
    Ok(Merged { doc: out, changed, record: next, conflicts })
}

/// One line per owned key that no longer holds what was last applied. Released keys and keys
/// that were never applied are not reported.
pub fn drift(doc: &Value, owns: &BTreeMap<String, Value>, record: &Record) -> Vec<String> {
    let mut notes = Vec::new();
    for (key, declared) in owns {
        let Some(last) = record.keys.get(key) else { continue };
        let Ok(want) = Want::parse(key, declared) else { continue };
        if record.released.get(key) == Some(&want.value) {
            continue;
        }
        match get(doc, key) {
            None => notes.push(format!("`{key}` was removed")),
            disk if !holds(&want.kind, disk, last) => notes.push(format!("`{key}` changed")),
            _ => {}
        }
    }
    notes
}
