//! Evaluates `spec.star` into hosts. Builtins return opaque `resource` values carrying their JSON
//! form, validated against the `fspec-core` types as soon as they are constructed, so a typo
//! fails at the line that made it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use allocative::Allocative;
use anyhow::{Context, anyhow, bail};
use fspec_core::facts::Facts;
use fspec_core::resource::{Host, Resource, Source, System};
use serde::de::DeserializeOwned;
use serde::{Serialize, Serializer};
use serde_json::{Map, Value as Json};
use starlark::environment::{FrozenModule, Globals, GlobalsBuilder, LibraryExtension, Module};
use starlark::eval::{Evaluator, FileLoader};
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::dict::DictRef;
use starlark::values::list::ListRef;
use starlark::values::structs::AllocStruct;
use starlark::values::tuple::{TupleRef, UnpackTuple};
use starlark::values::{Heap, ProvidesStaticType, StarlarkValue, Value, starlark_value};

/// The result of evaluating a spec: every host it declares, modules expanded.
#[derive(Debug)]
pub struct Spec {
    pub hosts: Vec<Host>,
}

impl Spec {
    pub fn host(&self, name: &str) -> anyhow::Result<&Host> {
        self.hosts.iter().find(|h| h.name == name).ok_or_else(|| {
            let names: Vec<_> = self.hosts.iter().map(|h| h.name.as_str()).collect();
            anyhow!("no host named `{name}` in the spec (hosts: {})", names.join(", "))
        })
    }
}

/// Evaluate `spec_path`. `load("//x.star", ...)` resolves against the spec's directory.
pub fn evaluate(spec_path: &Path) -> anyhow::Result<Spec> {
    let root = spec_path.parent().map(Path::to_path_buf).unwrap_or_default();
    let globals = globals();
    let loader =
        Loader { root: SpecRoot(root), globals: &globals, cache: RefCell::default(), loading: RefCell::default() };
    let text = std::fs::read_to_string(spec_path).with_context(|| format!("reading {}", spec_path.display()))?;
    let ast = AstModule::parse(&spec_path.display().to_string(), text, &dialect()).map_err(|e| e.into_anyhow())?;

    Module::with_temp_heap(|module| {
        let mut eval = Evaluator::new(&module);
        eval.set_loader(&loader);
        eval.extra = Some(&loader.root);
        eval.eval_module(ast, &globals).map_err(|e| e.into_anyhow())?;
        let hosts = module.get("hosts").context("spec.star must set `hosts` to a list of host(...)")?;
        let list = ListRef::from_value(hosts).context("`hosts` must be a list")?;
        let hosts = list
            .iter()
            .map(|v| match Resource_::from_value(v) {
                Some(r) if r.kind == "host" => Ok(serde_json::from_value(r.data.clone())?),
                _ => bail!("`hosts` entries must be host(...), got {}", v.get_type()),
            })
            .collect::<anyhow::Result<Vec<Host>>>()?;
        let mut seen = std::collections::HashSet::new();
        for h in &hosts {
            if !seen.insert(&h.name) {
                bail!("host `{}` is declared more than once", h.name);
            }
        }
        Ok(Spec { hosts })
    })
}

fn dialect() -> Dialect {
    Dialect::Extended
}

fn globals() -> Globals {
    GlobalsBuilder::extended_by(&[LibraryExtension::StructType, LibraryExtension::Json, LibraryExtension::Print])
        .with(builtins)
        .build()
}

/// The spec's directory, which `glob` reads and `load` resolves against.
#[derive(ProvidesStaticType)]
struct SpecRoot(PathBuf);

struct Loader<'a> {
    root: SpecRoot,
    globals: &'a Globals,
    cache: RefCell<HashMap<String, FrozenModule>>,
    loading: RefCell<Vec<String>>,
}

impl FileLoader for Loader<'_> {
    fn load(&self, path: &str) -> starlark::Result<FrozenModule> {
        if let Some(m) = self.cache.borrow().get(path) {
            return Ok(m.clone());
        }
        let Some(rel) = path.strip_prefix("//") else {
            return Err(other(anyhow!(
                "load(\"{path}\"): paths start with // and are relative to the spec's directory"
            )));
        };
        if self.loading.borrow().iter().any(|p| p == path) {
            return Err(other(anyhow!("load cycle: {} -> {path}", self.loading.borrow().join(" -> "))));
        }
        let file = self.root.0.join(rel);
        let text = std::fs::read_to_string(&file).with_context(|| format!("load(\"{path}\")")).map_err(other)?;
        let ast = AstModule::parse(path, text, &dialect())?;
        self.loading.borrow_mut().push(path.into());
        let frozen = Module::with_temp_heap(|module| {
            let mut eval = Evaluator::new(&module);
            eval.set_loader(self);
            eval.extra = Some(&self.root);
            eval.eval_module(ast, self.globals)?;
            drop(eval);
            module.freeze().map_err(|e| other(anyhow!("{e:?}")))
        });
        self.loading.borrow_mut().pop();
        let frozen = frozen?;
        self.cache.borrow_mut().insert(path.into(), frozen.clone());
        Ok(frozen)
    }
}

fn other(e: anyhow::Error) -> starlark::Error {
    starlark::Error::new_other(e)
}

/// What every builtin returns. `data` is the validated JSON form, `kind` included.
#[derive(Debug, Clone, ProvidesStaticType, Allocative)]
struct Resource_ {
    kind: String,
    #[allocative(skip)]
    data: Json,
}

starlark_simple_value!(Resource_);

impl fmt::Display for Resource_ {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}({})", self.kind, self.data)
    }
}

#[starlark_value(type = "resource")]
impl<'v> StarlarkValue<'v> for Resource_ {
    fn collect_repr(&self, collector: &mut String) {
        collector.push_str(&self.to_string());
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        self.data.get(attribute).map(|v| heap.alloc(v.clone()))
    }

    fn has_attr(&self, attribute: &str, _heap: Heap<'v>) -> bool {
        self.data.get(attribute).is_some()
    }

    fn write_hash(&self, hasher: &mut starlark::collections::StarlarkHasher) -> starlark::Result<()> {
        use std::hash::Hash;
        self.data.to_string().hash(hasher);
        Ok(())
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(Resource_::from_value(other).is_some_and(|o| o.data == self.data))
    }
}

/// Lets `to_json_value` descend into lists and dicts that hold resources.
impl Serialize for Resource_ {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.data.serialize(s)
    }
}

/// Map positional and keyword arguments onto field names, as JSON.
fn fields<'v>(what: &str, names: &[&str], args: &[Value<'v>], kwargs: Value<'v>) -> anyhow::Result<Map<String, Json>> {
    if args.len() > names.len() {
        bail!("{what}() takes at most {} positional argument(s), got {}", names.len(), args.len());
    }
    let mut map = Map::new();
    for (name, v) in names.iter().zip(args) {
        map.insert((*name).into(), to_json(what, name, *v)?);
    }
    if let Some(kw) = DictRef::from_value(kwargs) {
        for (k, v) in kw.iter() {
            let k = k.unpack_str().context("keyword names are strings")?.to_string();
            if map.contains_key(&k) {
                bail!("{what}() got `{k}` both positionally and by keyword");
            }
            let json = to_json(what, &k, v)?;
            map.insert(k, json);
        }
    }
    map.retain(|_, v| !v.is_null());
    Ok(map)
}

fn to_json(what: &str, field: &str, v: Value) -> anyhow::Result<Json> {
    v.to_json_value().map_err(|e| anyhow!("{what}({field} = ...): {e}"))
}

/// Validate `map` as `T`, then keep the normalized form (defaults filled in).
fn validated<T: DeserializeOwned + Serialize>(what: &str, map: Map<String, Json>) -> anyhow::Result<Json> {
    let typed: T = serde_json::from_value(Json::Object(map)).map_err(|e| anyhow!("{what}(): {e}"))?;
    Ok(serde_json::to_value(typed)?)
}

fn make<'v, T: DeserializeOwned + Serialize>(
    kind: &str,
    names: &[&str],
    args: UnpackTuple<Value<'v>>,
    kwargs: Value<'v>,
) -> anyhow::Result<Resource_> {
    let mut map = fields(kind, names, &args.items, kwargs)?;
    map.insert("kind".into(), kind.into());
    Ok(Resource_ { kind: kind.into(), data: validated::<T>(kind, map)? })
}

/// A struct-typed field (pkg files, host users) holds `kind`-tagged JSON; check and drop the tag.
fn untag(what: &str, field: &str, kind: &str, v: &mut Json) -> anyhow::Result<()> {
    let items: Vec<&mut Json> = match v {
        Json::Array(items) => items.iter_mut().collect(),
        _ => bail!("{what}({field} = ...) must be a list"),
    };
    for item in items {
        let obj = item.as_object_mut().with_context(|| format!("{what}({field} = ...) takes {kind}(...) values"))?;
        match obj.remove("kind") {
            Some(Json::String(k)) if k == kind => {}
            Some(k) => bail!("{what}({field} = ...) takes {kind}(...) values, got {k}"),
            None => bail!("{what}({field} = ...) takes {kind}(...) values"),
        }
    }
    Ok(())
}

fn pkg_<'v>(args: UnpackTuple<Value<'v>>, kwargs: Value<'v>) -> anyhow::Result<Resource_> {
    let mut map = fields("pkg", &["name"], &args.items, kwargs)?;
    let name = map.get("name").and_then(Json::as_str).context("pkg() needs a name")?.to_string();
    map.entry("source").or_insert_with(|| serde_json::to_value(Source::Aqua { name }).unwrap());
    for (field, kind) in [("files", "file"), ("services", "service"), ("timers", "timer")] {
        if let Some(v) = map.get_mut(field) {
            untag("pkg", field, kind, v)?;
        }
    }
    map.insert("kind".into(), "pkg".into());
    Ok(Resource_ { kind: "pkg".into(), data: validated::<Resource>("pkg", map)? })
}

fn distro_(distro: &str, default_init: &str, kwargs: Value) -> anyhow::Result<Resource_> {
    let mut map = fields(distro, &[], &[], kwargs)?;
    map.insert("kind".into(), "distro".into());
    map.insert("distro".into(), distro.into());
    map.entry("init").or_insert_with(|| default_init.into());
    Ok(Resource_ { kind: "distro".into(), data: validated::<System>(distro, map)? })
}

fn host_<'v>(
    args: UnpackTuple<Value<'v>>,
    kwargs: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<Resource_> {
    let mut modules = Value::new_none();
    let mut plain = Vec::new();
    if let Some(kw) = DictRef::from_value(kwargs) {
        for (k, v) in kw.iter() {
            if k.unpack_str() == Some("modules") {
                modules = v;
            } else {
                plain.push((k, v));
            }
        }
    }
    let heap = eval.heap();
    let rest = heap.alloc(starlark::values::dict::AllocDict(plain));
    let mut map = fields("host", &["name"], &args.items, rest)?;
    let name = map.get("name").and_then(Json::as_str).context("host() needs a name")?.to_string();
    let what = format!("host({name})");
    let platform = map
        .get("platform")
        .and_then(Json::as_str)
        .with_context(|| format!("{what}: platform is required, e.g. platform = \"x86_64-linux\""))?
        .to_string();
    let system: Option<System> = match map.get("system") {
        Some(s) => Some(serde_json::from_value(s.clone()).map_err(|e| anyhow!("{what}: system: {e}"))?),
        None => None,
    };
    let facts = Facts::declared(&name, &platform, system.as_ref())?;
    if let Some(users) = map.get_mut("users") {
        untag(&what, "users", "user", users)?;
    }

    let facts_value = facts_struct(heap, &facts);
    let mut resources = Vec::new();
    expand(&what, modules, facts_value, eval, &mut resources)?;
    map.insert("resources".into(), Json::Array(resources));
    let data = validated::<Host>(&what, map)?;
    Ok(Resource_ { kind: "host".into(), data })
}

fn facts_struct<'v>(heap: Heap<'v>, f: &Facts) -> Value<'v> {
    let is_darwin = f.os == fspec_core::facts::Os::Darwin;
    let platform = heap.alloc(AllocStruct([
        ("name", heap.alloc(f.platform())),
        ("os", heap.alloc(f.os.to_string())),
        ("arch", heap.alloc(f.arch.as_str())),
        ("is_darwin", Value::new_bool(is_darwin)),
        ("is_linux", Value::new_bool(!is_darwin)),
    ]));
    heap.alloc(AllocStruct([
        ("name", heap.alloc(f.name.as_str())),
        ("platform", platform),
        ("distro", heap.alloc(f.distro.as_str())),
        ("init", heap.alloc(f.init.to_string())),
    ]))
}

/// Flatten a `modules` value: resources, lists of them, `None`, and functions called with the host.
fn expand<'v>(
    what: &str,
    v: Value<'v>,
    facts: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
    out: &mut Vec<Json>,
) -> anyhow::Result<()> {
    if v.is_none() {
        return Ok(());
    }
    if let Some(r) = Resource_::from_value(v) {
        match r.kind.as_str() {
            "pkg" | "file" | "service" | "timer" | "user" | "secret" | "defaults" => {}
            "image" | "distro" => bail!("{what}: {}(...) goes in host(system = ...), not in modules", r.kind),
            kind => bail!("{what}: modules hold resources, got {kind}(...)"),
        }
        out.push(r.data.clone());
        return Ok(());
    }
    let items = ListRef::from_value(v).map(|l| l.content()).or_else(|| TupleRef::from_value(v).map(|t| t.content()));
    if let Some(items) = items {
        for item in items.iter().copied() {
            expand(what, item, facts, eval, out)?;
        }
        return Ok(());
    }
    if v.get_type() == "function" {
        let result = eval.eval_function(v, &[facts], &[]).map_err(|e| e.into_anyhow())?;
        return expand(what, result, facts, eval, out);
    }
    bail!("{what}: modules hold resources, lists or functions, got {}", v.get_type())
}

/// The one place evaluation reads the filesystem besides `load`: files under the spec's own
/// directory tree, so a spec stays a function of the repo. Sorted, `/`-separated, relative to
/// the spec directory; directories are skipped and links leading outside the tree are dropped.
fn glob_(root: &Path, pattern: &str) -> anyhow::Result<Vec<String>> {
    use std::path::Component;
    if Path::new(pattern).components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
        bail!(
            "glob(\"{pattern}\"): patterns are relative to the spec's directory and cannot use `..` or start with `/`"
        );
    }
    let canonical = root.canonicalize().with_context(|| format!("reading {}", root.display()))?;
    let full = format!("{}/{pattern}", glob::Pattern::escape(&root.to_string_lossy()));
    let mut found = Vec::new();
    for entry in glob::glob(&full).map_err(|e| anyhow!("glob(\"{pattern}\"): {e}"))? {
        let path = entry?;
        if !path.is_file() || !path.canonicalize().is_ok_and(|p| p.starts_with(&canonical)) {
            continue;
        }
        let rel = path.strip_prefix(root).context("glob matched outside the spec directory")?;
        found.push(rel.to_string_lossy().replace('\\', "/"));
    }
    found.sort();
    Ok(found)
}

#[starlark_module]
fn builtins(builder: &mut GlobalsBuilder) {
    fn glob<'v>(pattern: &str, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Vec<String>> {
        let root = eval.extra.and_then(|e| e.downcast_ref::<SpecRoot>()).context("glob needs a spec directory")?;
        glob_(&root.0, pattern)
    }

    fn host<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Resource_> {
        host_(args, kwargs, eval)
    }

    fn pkg<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        pkg_(args, kwargs)
    }

    fn file<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Resource>("file", &["path"], args, kwargs)
    }

    fn service<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Resource>("service", &["name"], args, kwargs)
    }

    fn timer<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Resource>("timer", &["name"], args, kwargs)
    }

    fn user<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Resource>("user", &["name"], args, kwargs)
    }

    fn secret<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Resource>("secret", &["path"], args, kwargs)
    }

    fn defaults<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Resource>("defaults", &["domain", "key", "value"], args, kwargs)
    }

    fn image<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<System>("image", &["base"], args, kwargs)
    }

    fn fedora<'v>(#[starlark(kwargs)] kwargs: Value<'v>) -> anyhow::Result<Resource_> {
        distro_("fedora", "systemd", kwargs)
    }

    fn arch<'v>(#[starlark(kwargs)] kwargs: Value<'v>) -> anyhow::Result<Resource_> {
        distro_("arch", "systemd", kwargs)
    }

    fn debian<'v>(#[starlark(kwargs)] kwargs: Value<'v>) -> anyhow::Result<Resource_> {
        distro_("debian", "systemd", kwargs)
    }

    fn void<'v>(#[starlark(kwargs)] kwargs: Value<'v>) -> anyhow::Result<Resource_> {
        distro_("void", "runit", kwargs)
    }

    fn aqua<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("aqua", &["name"], args, kwargs)
    }

    fn github<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("github", &["repo"], args, kwargs)
    }

    fn npm<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("npm", &["name"], args, kwargs)
    }

    fn cargo<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("cargo", &["name"], args, kwargs)
    }

    fn flatpak<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("flatpak", &["id"], args, kwargs)
    }

    fn brew<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("brew", &["name"], args, kwargs)
    }

    fn dnf<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("dnf", &["names"], args, kwargs)
    }

    fn pacman<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("pacman", &["names"], args, kwargs)
    }

    fn apt<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("apt", &["names"], args, kwargs)
    }

    fn xbps<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("xbps", &["names"], args, kwargs)
    }

    fn git<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("git", &["url", "rev"], args, kwargs)
    }

    fn nix<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("nix", &["attr"], args, kwargs)
    }

    fn list_union<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<fspec_core::resource::ListStrategy>("list_union", &["items"], args, kwargs)
    }

    fn list_replace<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<fspec_core::resource::ListStrategy>("list_replace", &["items"], args, kwargs)
    }

    fn list_keyed<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<fspec_core::resource::ListStrategy>("list_keyed", &["field", "items"], args, kwargs)
    }
}

#[cfg(test)]
mod tests;
