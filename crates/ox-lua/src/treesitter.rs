//! Tree-sitter's C-facing Lua API.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use libloading::Library;
use mlua::{AnyUserData, Function, IntoLua, Lua, MetaMethod, MultiValue, Table, UserData, UserDataMethods, Value, Variadic};
use tree_sitter::{
    InputEdit, Language, LogType, Node, ParseOptions, Parser, Point, Query, QueryCursor, Range,
    StreamingIterator, Tree,
};
use tree_sitter_language::LanguageFn;

use crate::vim::Scheduler;

struct LoadedLanguage {
    language: Language,
    // A generated language is data owned by its dynamic library. This field must
    // therefore be dropped after every Language clone, parser, tree, and query.
    _library: Library,
}

type Languages = Rc<RefCell<HashMap<String, Arc<LoadedLanguage>>>>;

struct ParserHandle {
    parser: Parser,
    language: Arc<LoadedLanguage>,
    scheduler: Rc<dyn Scheduler>,
    logger: Option<Function>,
    logger_error: Rc<RefCell<Option<String>>>,
}

#[derive(Clone)]
struct TreeHandle(Arc<TreeData>);

struct TreeData {
    tree: Tree,
    source: Arc<[u8]>,
    _language: Arc<LoadedLanguage>,
}

#[derive(Clone)]
struct NodeHandle {
    tree: TreeHandle,
    path: Vec<u32>,
}

struct QueryHandle {
    query: Query,
    _language: Arc<LoadedLanguage>,
}

#[derive(Clone)]
struct MatchHandle {
    id: u32,
    pattern_index: usize,
    captures: Vec<(u32, NodeHandle)>,
}

struct CursorHandle {
    matches: Vec<MatchHandle>,
    captures: Vec<(u32, NodeHandle, MatchHandle)>,
    next_match: usize,
    next_capture: usize,
    removed: HashSet<u32>,
}

fn runtime_error(message: impl Into<String>) -> mlua::Error {
    mlua::Error::runtime(message.into())
}

fn checked_u32(value: i64, what: &str) -> mlua::Result<u32> {
    u32::try_from(value).map_err(|_| runtime_error(format!("{what} out of bounds")))
}

fn point(row: i64, column: i64) -> mlua::Result<Point> {
    Ok(Point::new(
        usize::try_from(row).map_err(|_| runtime_error("row out of bounds"))?,
        usize::try_from(column).map_err(|_| runtime_error("column out of bounds"))?,
    ))
}

fn range_table(lua: &Lua, range: Range, include_bytes: bool) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    if include_bytes {
        table.raw_set(1, range.start_point.row)?;
        table.raw_set(2, range.start_point.column)?;
        table.raw_set(3, range.start_byte)?;
        table.raw_set(4, range.end_point.row)?;
        table.raw_set(5, range.end_point.column)?;
        table.raw_set(6, range.end_byte)?;
    } else {
        table.raw_set(1, range.start_point.row)?;
        table.raw_set(2, range.start_point.column)?;
        table.raw_set(3, range.end_point.row)?;
        table.raw_set(4, range.end_point.column)?;
    }
    Ok(table)
}

fn ranges_table(lua: &Lua, ranges: impl IntoIterator<Item = Range>, include_bytes: bool) -> mlua::Result<Table> {
    let result = lua.create_table()?;
    for (index, range) in ranges.into_iter().enumerate() {
        result.raw_set(index + 1, range_table(lua, range, include_bytes)?)?;
    }
    Ok(result)
}

fn range_from_value(value: Value) -> mlua::Result<Range> {
    match value {
        Value::Table(table) if table.raw_len() == 6 => Ok(Range {
            start_point: point(table.raw_get(1)?, table.raw_get(2)?)?,
            start_byte: usize::try_from(table.raw_get::<i64>(3)?)
                .map_err(|_| runtime_error("Range value out of bounds"))?,
            end_point: point(table.raw_get(4)?, table.raw_get(5)?)?,
            end_byte: usize::try_from(table.raw_get::<i64>(6)?)
                .map_err(|_| runtime_error("Range value out of bounds"))?,
        }),
        Value::UserData(ud) => Ok(ud.borrow::<NodeHandle>()?.resolve()?.range()),
        _ => Err(runtime_error(
            "Ranges can only be made from 6 element long tables or nodes.",
        )),
    }
}

fn parse_deadline_callback(
    started: Instant,
    deadline: Duration,
) -> impl FnMut(&tree_sitter::ParseState) -> ControlFlow<()> {
    move |_| {
        if started.elapsed() >= deadline {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }
}

impl NodeHandle {
    fn resolve(&self) -> mlua::Result<Node<'_>> {
        let mut node = self.tree.0.tree.root_node();
        for &index in &self.path {
            node = node
                .child(index)
                .ok_or_else(|| runtime_error("tree node is no longer available"))?;
        }
        Ok(node)
    }

    fn from_node(tree: TreeHandle, node: Node<'_>) -> mlua::Result<Self> {
        let mut current = node;
        let mut path = Vec::new();
        while let Some(parent) = current.parent() {
            let index = (0..parent.child_count())
                .find(|&index| parent.child(index as u32).is_some_and(|child| child == current))
                .ok_or_else(|| runtime_error("failed to locate node in its tree"))?;
            path.push(u32::try_from(index).map_err(|_| runtime_error("node path is too deep"))?);
            current = parent;
        }
        path.reverse();
        Ok(Self { tree, path })
    }

    fn related(&self, node: Option<Node<'_>>) -> mlua::Result<Option<Self>> {
        node.map(|node| Self::from_node(self.tree.clone(), node)).transpose()
    }
}

impl UserData for ParserHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_, _, ()| Ok("<parser>"));
        methods.add_method_mut("reset", |_, this, ()| {
            this.parser.reset();
            Ok(())
        });
        methods.add_method_mut("set_included_ranges", |_, this, values: Table| {
            let ranges = values
                .sequence_values::<Value>()
                .map(|value| value.and_then(range_from_value))
                .collect::<mlua::Result<Vec<_>>>()?;
            this.parser
                .set_included_ranges(&ranges)
                .map_err(|error| runtime_error(error.to_string()))
        });
        methods.add_method("included_ranges", |lua, this, include_bytes: Option<bool>| {
            ranges_table(lua, this.parser.included_ranges(), include_bytes.unwrap_or(false))
        });
        methods.add_method_mut(
            "parse",
            |lua, this, (old, input, include_bytes, timeout): (Option<AnyUserData>, Value, Option<bool>, Option<u64>)| {
                let bytes = match input {
                    Value::String(string) => string.as_bytes().to_vec(),
                    Value::Integer(_) | Value::Number(_) => {
                        return Err(runtime_error("expected either string or buffer handle; buffer parsing is unavailable"));
                    }
                    _ => return Err(runtime_error("expected either string or buffer handle")),
                };
                let old_tree = old
                    .as_ref()
                    .map(|value| value.borrow::<TreeHandle>())
                    .transpose()?;
                let old_tree_ref = old_tree.as_ref().map(|tree| &tree.0.tree);
                let timeout = timeout.unwrap_or(0);
                let parsed = if timeout == 0 {
                    this.parser.parse(&bytes, old_tree_ref)
                } else {
                    let started = Instant::now();
                    let deadline = Duration::from_nanos(timeout);
                    let length = bytes.len();
                    let mut input = |offset: usize, _: Point| {
                        (offset < length).then(|| &bytes[offset..]).unwrap_or_default()
                    };
                    let mut progress = parse_deadline_callback(started, deadline);
                    let options = ParseOptions::new().progress_callback(&mut progress);
                    this.parser.parse_with_options(&mut input, old_tree_ref, Some(options))
                }
                .ok_or_else(|| runtime_error("Language was unset, has an incompatible ABI, or parsing timed out."))?;
                if let Some(message) = this.logger_error.borrow_mut().take() {
                    return Err(runtime_error(message));
                }
                let changed = if let Some(old_tree) = old_tree.as_ref() {
                    old_tree.0.tree.changed_ranges(&parsed).collect::<Vec<_>>()
                } else {
                    parsed.included_ranges()
                };
                let tree = TreeHandle(Arc::new(TreeData {
                    tree: parsed,
                    source: Arc::from(bytes),
                    _language: this.language.clone(),
                }));
                Ok((tree, ranges_table(lua, changed, include_bytes.unwrap_or(false))?))
            },
        );
        methods.add_method_mut(
            "_set_logger",
            |_, this, (lex, parse, callback): (bool, bool, Function)| {
                let scheduler = this.scheduler.clone();
                let callback_for_log = callback.clone();
                let error = this.logger_error.clone();
                this.parser.set_logger(Some(Box::new(move |kind, message| {
                    let enabled = match kind {
                        LogType::Lex => lex,
                        LogType::Parse => parse,
                    };
                    if !enabled {
                        return;
                    }
                    let callback = callback_for_log.clone();
                    let kind = match kind { LogType::Lex => "lex", LogType::Parse => "parse" };
                    let message = message.to_owned();
                    if let Err(schedule_error) = scheduler.schedule_deferred(Box::new(move || {
                        callback.call::<()>((kind, message))
                    })) {
                        *error.borrow_mut() = Some(format!("treesitter logger callback scheduling failed: {schedule_error}"));
                    }
                })));
                this.logger = Some(callback);
                Ok(())
            },
        );
        methods.add_method("_logger", |_, this, ()| Ok(this.logger.clone()));
    }
}

impl UserData for TreeHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_, _, ()| Ok("<tree>"));
        methods.add_meta_method(MetaMethod::Eq, |_, this, other: AnyUserData| {
            let other = other.borrow::<TreeHandle>()?;
            Ok(Arc::ptr_eq(&this.0, &other.0))
        });
        methods.add_method("copy", |_, this, ()| {
            Ok(TreeHandle(Arc::new(TreeData {
                tree: this.0.tree.clone(),
                source: this.0.source.clone(),
                _language: this.0._language.clone(),
            })))
        });
        methods.add_method("root", |_, this, ()| Ok(NodeHandle { tree: this.clone(), path: Vec::new() }));
        methods.add_method("included_ranges", |lua, this, include_bytes: Option<bool>| {
            ranges_table(lua, this.0.tree.included_ranges(), include_bytes.unwrap_or(false))
        });
        methods.add_method(
            "edit",
            |_, this, args: Variadic<i64>| {
                if args.len() != 9 {
                    return Err(runtime_error("not enough args to tree:edit()"));
                }
                let mut tree = this.0.tree.clone();
                tree.edit(&InputEdit {
                    start_byte: usize::try_from(args[0]).map_err(|_| runtime_error("start byte out of bounds"))?,
                    old_end_byte: usize::try_from(args[1]).map_err(|_| runtime_error("old end byte out of bounds"))?,
                    new_end_byte: usize::try_from(args[2]).map_err(|_| runtime_error("new end byte out of bounds"))?,
                    start_position: point(args[3], args[4])?,
                    old_end_position: point(args[5], args[6])?,
                    new_end_position: point(args[7], args[8])?,
                });
                Ok(TreeHandle(Arc::new(TreeData {
                    tree,
                    source: this.0.source.clone(),
                    _language: this.0._language.clone(),
                })))
            },
        );
    }
}

fn push_optional_node(value: Option<NodeHandle>) -> Option<NodeHandle> { value }

impl UserData for NodeHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| Ok(format!("<node {}>", this.resolve()?.kind())));
        methods.add_meta_method(MetaMethod::Eq, |_, this, other: AnyUserData| {
            let other = other.borrow::<NodeHandle>()?;
            Ok(Arc::ptr_eq(&this.tree.0, &other.tree.0) && this.path == other.path)
        });
        methods.add_meta_method(MetaMethod::Len, |_, this, ()| Ok(this.resolve()?.child_count()));
        methods.add_method("id", |lua, this, ()| Ok(lua.create_string(this.resolve()?.id().to_ne_bytes())?));
        methods.add_method("range", |lua, this, include_bytes: Option<bool>| {
            let range = this.resolve()?.range();
            if include_bytes.unwrap_or(false) {
                Ok(MultiValue::from_vec(vec![
                    range.start_point.row.into_lua(lua)?,
                    range.start_point.column.into_lua(lua)?,
                    range.start_byte.into_lua(lua)?,
                    range.end_point.row.into_lua(lua)?,
                    range.end_point.column.into_lua(lua)?,
                    range.end_byte.into_lua(lua)?,
                ]))
            } else {
                Ok(MultiValue::from_vec(vec![
                    range.start_point.row.into_lua(lua)?,
                    range.start_point.column.into_lua(lua)?,
                    range.end_point.row.into_lua(lua)?,
                    range.end_point.column.into_lua(lua)?,
                ]))
            }
        });
        methods.add_method("start", |_, this, ()| { let n = this.resolve()?; let p = n.start_position(); Ok((p.row, p.column, n.start_byte())) });
        methods.add_method("end_", |_, this, ()| { let n = this.resolve()?; let p = n.end_position(); Ok((p.row, p.column, n.end_byte())) });
        methods.add_method("type", |_, this, ()| Ok(this.resolve()?.kind().to_owned()));
        methods.add_method("symbol", |_, this, ()| Ok(this.resolve()?.kind_id()));
        methods.add_method("named", |_, this, ()| Ok(this.resolve()?.is_named()));
        methods.add_method("missing", |_, this, ()| Ok(this.resolve()?.is_missing()));
        methods.add_method("extra", |_, this, ()| Ok(this.resolve()?.is_extra()));
        methods.add_method("has_changes", |_, this, ()| Ok(this.resolve()?.has_changes()));
        methods.add_method("has_error", |_, this, ()| Ok(this.resolve()?.has_error()));
        methods.add_method("sexpr", |_, this, ()| Ok(this.resolve()?.to_sexp()));
        methods.add_method("child_count", |_, this, ()| Ok(this.resolve()?.child_count()));
        methods.add_method("named_child_count", |_, this, ()| Ok(this.resolve()?.named_child_count()));
        methods.add_method("byte_length", |_, this, ()| { let n = this.resolve()?; Ok(n.end_byte() - n.start_byte()) });
        methods.add_method("tree", |_, this, ()| Ok(this.tree.clone()));
        methods.add_method("root", |_, this, ()| Ok(NodeHandle { tree: this.tree.clone(), path: Vec::new() }));
        methods.add_method("equal", |_, this, other: AnyUserData| { let other = other.borrow::<NodeHandle>()?; Ok(Arc::ptr_eq(&this.tree.0, &other.tree.0) && this.path == other.path) });
        methods.add_method("child", |_, this, index: i64| { let index = checked_u32(index, "child index")?; let node = this.resolve()?.child(index); Ok(push_optional_node(this.related(node)?)) });
        methods.add_method("named_child", |_, this, index: i64| { let index = checked_u32(index, "child index")?; let node = this.resolve()?.named_child(index); Ok(push_optional_node(this.related(node)?)) });
        methods.add_method("parent", |_, this, ()| { let node = this.resolve()?.parent(); Ok(push_optional_node(this.related(node)?)) });
        methods.add_method("next_sibling", |_, this, ()| { let node = this.resolve()?.next_sibling(); Ok(push_optional_node(this.related(node)?)) });
        methods.add_method("prev_sibling", |_, this, ()| { let node = this.resolve()?.prev_sibling(); Ok(push_optional_node(this.related(node)?)) });
        methods.add_method("next_named_sibling", |_, this, ()| { let node = this.resolve()?.next_named_sibling(); Ok(push_optional_node(this.related(node)?)) });
        methods.add_method("prev_named_sibling", |_, this, ()| { let node = this.resolve()?.prev_named_sibling(); Ok(push_optional_node(this.related(node)?)) });
        methods.add_method("descendant_for_range", |_, this, (sr, sc, er, ec): (i64, i64, i64, i64)| { let node = this.resolve()?.descendant_for_point_range(point(sr, sc)?, point(er, ec)?); Ok(push_optional_node(this.related(node)?)) });
        methods.add_method("named_descendant_for_range", |_, this, (sr, sc, er, ec): (i64, i64, i64, i64)| { let node = this.resolve()?.named_descendant_for_point_range(point(sr, sc)?, point(er, ec)?); Ok(push_optional_node(this.related(node)?)) });
        methods.add_method("child_with_descendant", |_, this, descendant: AnyUserData| {
            let descendant = descendant.borrow::<NodeHandle>()?;
            if !Arc::ptr_eq(&this.tree.0, &descendant.tree.0) { return Ok(None); }
            let node = this.resolve()?.child_with_descendant(descendant.resolve()?);
            this.related(node)
        });
        methods.add_method("field", |_, this, name: String| {
            let node = this.resolve()?;
            let mut result = Vec::new();
            for index in 0..node.child_count() {
                if node.field_name_for_child(index as u32) == Some(name.as_str()) {
                    if let Some(child) = node.child(index as u32) { result.push(NodeHandle::from_node(this.tree.clone(), child)?); }
                }
            }
            Ok(result)
        });
        methods.add_method("named_children", |_, this, ()| {
            let node = this.resolve()?;
            let mut result = Vec::new();
            for index in 0..node.named_child_count() {
                if let Some(child) = node.named_child(index as u32) { result.push(NodeHandle::from_node(this.tree.clone(), child)?); }
            }
            Ok(result)
        });
        methods.add_method("iter_children", |lua, this, ()| {
            let source = this.clone();
            let index = Rc::new(Cell::new(0u32));
            lua.create_function_mut(move |_, ()| {
                let current = index.get();
                let node = source.resolve()?;
                let Some(child) = node.child(current) else {
                    return Ok((None, None));
                };
                index.set(current.saturating_add(1));
                let field = node.field_name_for_child(current).map(str::to_owned);
                Ok((Some(NodeHandle::from_node(source.tree.clone(), child)?), field))
            })
        });
        methods.add_method("__has_ancestor", |_, this, predicate: Table| {
            let types = predicate.sequence_values::<String>().skip(2).collect::<mlua::Result<HashSet<_>>>()?;
            let mut node = this.resolve()?;
            while let Some(parent) = node.parent() {
                if types.contains(parent.kind()) { return Ok(true); }
                node = parent;
            }
            Ok(false)
        });
    }
}

impl UserData for QueryHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_, _, ()| Ok("<query>"));
        methods.add_method_mut("disable_capture", |_, this, name: String| { this.query.disable_capture(&name); Ok(()) });
        methods.add_method_mut("disable_pattern", |_, this, index: i64| {
            let index = usize::try_from(index).map_err(|_| runtime_error("pattern index out of bounds"))?;
            if index == 0 || index > this.query.pattern_count() { return Err(runtime_error("pattern index out of bounds")); }
            this.query.disable_pattern(index - 1);
            Ok(())
        });
        methods.add_method("inspect", |lua, this, ()| {
            let result = lua.create_table()?;
            let captures = lua.create_table()?;
            for (index, name) in this.query.capture_names().iter().enumerate() { captures.raw_set(index + 1, *name)?; }
            result.set("captures", captures)?;
            let patterns = lua.create_table()?;
            for index in 0..this.query.pattern_count() {
                let predicates = lua.create_table()?;
                for (pred_index, predicate) in this.query.general_predicates(index).iter().enumerate() {
                    let values = lua.create_table()?;
                    values.raw_set(1, predicate.operator.as_ref())?;
                    for (arg_index, arg) in predicate.args.iter().enumerate() {
                        match arg {
                            tree_sitter::QueryPredicateArg::Capture(id) => values.raw_set(arg_index + 2, usize::try_from(*id).map_err(|_| runtime_error("capture id out of bounds"))? + 1)?,
                            tree_sitter::QueryPredicateArg::String(value) => values.raw_set(arg_index + 2, value.as_ref())?,
                        }
                    }
                    predicates.raw_set(pred_index + 1, values)?;
                }
                patterns.raw_set(index + 1, predicates)?;
            }
            result.set("patterns", patterns)?;
            Ok(result)
        });
    }
}

impl UserData for MatchHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("info", |_, this, ()| Ok((this.id, this.pattern_index + 1)));
        methods.add_method("captures", |lua, this, ()| {
            let result = lua.create_table()?;
            for (capture, node) in &this.captures {
                let index = usize::try_from(*capture).map_err(|_| runtime_error("capture id out of bounds"))? + 1;
                let nodes = match result.raw_get::<Value>(index)? {
                    Value::Table(table) => table,
                    _ => lua.create_table()?,
                };
                nodes.raw_set(nodes.raw_len() + 1, node.clone())?;
                result.raw_set(index, nodes)?;
            }
            Ok(result)
        });
    }
}

impl UserData for CursorHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("remove_match", |_, this, id: i64| { this.removed.insert(checked_u32(id, "match id")?); Ok(()) });
        methods.add_method_mut("next_match", |_, this, ()| {
            while let Some(value) = this.matches.get(this.next_match).cloned() {
                this.next_match += 1;
                if !this.removed.contains(&value.id) { return Ok(Some(value)); }
            }
            Ok(None)
        });
        methods.add_method_mut("next_capture", |_, this, ()| {
            while let Some((index, node, matched)) = this.captures.get(this.next_capture).cloned() {
                this.next_capture += 1;
                if !this.removed.contains(&matched.id) {
                    return Ok((Some(index + 1), Some(node), Some(matched)));
                }
            }
            Ok((None, None, None))
        });
    }
}

fn load_language(path: &str, symbol: &str) -> mlua::Result<LoadedLanguage> {
    let symbol_name = format!("tree_sitter_{symbol}");
    // SAFETY: `Library::new` loads the caller-selected parser object, and `get`
    // requests the tree-sitter grammar ABI's generated zero-argument language
    // function. `LanguageFn::from_raw` has precisely that contract. The Library
    // is moved into LoadedLanguage and retained until all Language users drop,
    // so the returned grammar data and function code cannot be unloaded early.
    let (library, language) = unsafe {
        let library = Library::new(Path::new(path)).map_err(|error| runtime_error(format!("Failed to load parser: {error}")))?;
        let function = library
            .get::<unsafe extern "C" fn() -> *const ()>(symbol_name.as_bytes())
            .map_err(|error| runtime_error(format!("Failed to load parser: {error}")))?;
        let language = Language::new(LanguageFn::from_raw(*function));
        (library, language)
    };
    let version = language.abi_version();
    if !(tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION..=tree_sitter::LANGUAGE_VERSION).contains(&version) {
        return Err(runtime_error(format!(
            "ABI version mismatch for {path}: supported between {} and {}, found {version}",
            tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION,
            tree_sitter::LANGUAGE_VERSION,
        )));
    }
    Ok(LoadedLanguage { language, _library: library })
}

/// Install Neovim's tree-sitter C-facing fields on the existing `vim` table.
pub(crate) fn install(lua: &Lua, scheduler: Rc<dyn Scheduler>) -> mlua::Result<()> {
    let vim: Table = lua.globals().get("vim")?;
    let languages: Languages = Rc::new(RefCell::new(HashMap::new()));

    let registry = languages.clone();
    vim.set("_ts_add_language_from_object", lua.create_function(move |_, (path, name, symbol): (String, String, Option<String>)| {
        if registry.borrow().contains_key(&name) { return Ok(true); }
        let loaded = Arc::new(load_language(&path, symbol.as_deref().unwrap_or(&name))?);
        registry.borrow_mut().insert(name, loaded);
        Ok(true)
    })?)?;
    let registry = languages.clone();
    vim.set("_ts_has_language", lua.create_function(move |_, name: String| Ok(registry.borrow().contains_key(&name)))?)?;
    let registry = languages.clone();
    vim.set("_ts_remove_language", lua.create_function(move |_, name: String| Ok(registry.borrow_mut().remove(&name).is_some()))?)?;

    let registry = languages.clone();
    vim.set("_create_ts_parser", lua.create_function(move |_, name: String| {
        let language = registry.borrow().get(&name).cloned().ok_or_else(|| runtime_error(format!("no such language: {name}")))?;
        let mut parser = Parser::new();
        parser.set_language(&language.language).map_err(|error| runtime_error(format!("Failed to load language : {name}: {error}")))?;
        Ok(ParserHandle { parser, language, scheduler: scheduler.clone(), logger: None, logger_error: Rc::new(RefCell::new(None)) })
    })?)?;

    let registry = languages.clone();
    vim.set("_ts_parse_query", lua.create_function(move |_, (name, source): (String, String)| {
        let language = registry.borrow().get(&name).cloned().ok_or_else(|| runtime_error(format!("no such language: {name}")))?;
        let query = Query::new(&language.language, &source).map_err(|error| runtime_error(error.to_string()))?;
        Ok(QueryHandle { query, _language: language })
    })?)?;

    let registry = languages.clone();
    vim.set("_ts_inspect_language", lua.create_function(move |lua, name: String| {
        let language = registry.borrow().get(&name).cloned().ok_or_else(|| runtime_error(format!("no such language: {name}")))?;
        let result = lua.create_table()?;
        let symbols = lua.create_table()?;
        for id in 0..language.language.node_kind_count() {
            if let Some(kind) = language.language.node_kind_for_id(id as u16) {
                let key = if language.language.node_kind_is_named(id as u16) { kind.to_owned() } else { format!("\"{kind}\"") };
                symbols.set(key, language.language.node_kind_is_named(id as u16))?;
            }
        }
        result.set("symbols", symbols)?;
        let fields = lua.create_table()?;
        for id in 1..=language.language.field_count() {
            if let Some(field) = language.language.field_name_for_id(id as u16) { fields.raw_set(id, field)?; }
        }
        result.set("fields", fields)?;
        result.set("_wasm", false)?;
        result.set("abi_version", language.language.abi_version())?;
        result.set("state_count", language.language.parse_state_count())?;
        if let Some(metadata) = language.language.metadata() {
            let table = lua.create_table()?;
            table.set("major_version", metadata.major_version)?;
            table.set("minor_version", metadata.minor_version)?;
            table.set("patch_version", metadata.patch_version)?;
            result.set("metadata", table)?;
        }
        let supertypes = lua.create_table()?;
        for &supertype in language.language.supertypes() {
            let children = lua.create_table()?;
            for (index, &subtype) in language.language.subtypes_for_supertype(supertype).iter().enumerate() {
                if let Some(kind) = language.language.node_kind_for_id(subtype) { children.raw_set(index + 1, kind)?; }
            }
            if let Some(kind) = language.language.node_kind_for_id(supertype) { supertypes.set(kind, children)?; }
        }
        result.set("supertypes", supertypes)?;
        Ok(result)
    })?)?;

    vim.set("_create_ts_querycursor", lua.create_function(move |_, (node, query, options): (AnyUserData, AnyUserData, Option<Table>)| {
        let node = node.borrow::<NodeHandle>()?;
        let query = query.borrow::<QueryHandle>()?;
        let resolved = node.resolve()?;
        let mut cursor = QueryCursor::new();
        if let Some(options) = &options {
            let start = Point::new(options.get::<Option<usize>>("start_row")?.unwrap_or(0), options.get::<Option<usize>>("start_col")?.unwrap_or(0));
            let end = Point::new(options.get::<Option<usize>>("end_row")?.unwrap_or(usize::MAX), options.get::<Option<usize>>("end_col")?.unwrap_or(usize::MAX));
            cursor.set_point_range(start..end);
            if let Some(limit) = options.get::<Option<u32>>("match_limit")? { cursor.set_match_limit(limit); }
            if let Some(depth) = options.get::<Option<u32>>("max_start_depth")? { cursor.set_max_start_depth(Some(depth)); }
        }
        let mut matches = Vec::new();
        let mut iterator = cursor.matches(&query.query, resolved, node.tree.0.source.as_ref());
        while let Some(matched) = iterator.next() {
            let captures = matched.captures.iter().map(|capture| Ok((capture.index, NodeHandle::from_node(node.tree.clone(), capture.node)?))).collect::<mlua::Result<Vec<_>>>()?;
            matches.push(MatchHandle { id: matched.id(), pattern_index: matched.pattern_index, captures });
        }
        let mut capture_cursor = QueryCursor::new();
        if let Some(options) = &options {
            let start = Point::new(options.get::<Option<usize>>("start_row")?.unwrap_or(0), options.get::<Option<usize>>("start_col")?.unwrap_or(0));
            let end = Point::new(options.get::<Option<usize>>("end_row")?.unwrap_or(usize::MAX), options.get::<Option<usize>>("end_col")?.unwrap_or(usize::MAX));
            capture_cursor.set_point_range(start..end);
            if let Some(limit) = options.get::<Option<u32>>("match_limit")? { capture_cursor.set_match_limit(limit); }
            if let Some(depth) = options.get::<Option<u32>>("max_start_depth")? { capture_cursor.set_max_start_depth(Some(depth)); }
        }
        let mut captures = Vec::new();
        let mut iterator = capture_cursor.captures(&query.query, resolved, node.tree.0.source.as_ref());
        while let Some((matched, capture_index)) = iterator.next() {
            let all = matched.captures.iter().map(|capture| Ok((capture.index, NodeHandle::from_node(node.tree.clone(), capture.node)?))).collect::<mlua::Result<Vec<_>>>()?;
            let capture = matched.captures[*capture_index];
            captures.push((capture.index, NodeHandle::from_node(node.tree.clone(), capture.node)?, MatchHandle { id: matched.id(), pattern_index: matched.pattern_index, captures: all }));
        }
        Ok(CursorHandle { matches, captures, next_match: 0, next_capture: 0, removed: HashSet::new() })
    })?)?;

    vim.set("_ts_get_language_version", lua.create_function(|_, ()| Ok(tree_sitter::LANGUAGE_VERSION))?)?;
    vim.set("_ts_get_minimum_language_version", lua.create_function(|_, ()| Ok(tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION))?)?;
    Ok(())
}
