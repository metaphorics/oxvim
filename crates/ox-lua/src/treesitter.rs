//! Tree-sitter's C-facing Lua API.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use libloading::Library;
use mlua::{
    AnyUserData, FromLuaMulti, Function, IntoLua, IntoLuaMulti, Lua, MetaMethod, MultiValue, Table,
    UserData, UserDataMethods, Value, Variadic,
};
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
    /// Set by `__gc`; every method refuses work afterwards, mirroring
    /// upstream `parser_check` (`treesitter.c:423`).
    deleted: bool,
}

fn check_parser_live(handle: &ParserHandle) -> mlua::Result<()> {
    if handle.deleted {
        return Err(runtime_error("Parser has been deleted"));
    }
    Ok(())
}

#[derive(Clone)]
struct TreeHandle(Arc<TreeData>);

struct TreeData {
    tree: Tree,
    source: Arc<[u8]>,
    language: Arc<LoadedLanguage>,
}

#[derive(Clone)]
struct NodeHandle {
    tree: TreeHandle,
    path: Vec<u32>,
}

struct QueryHandle {
    query: Query,
    _language: Arc<LoadedLanguage>,
    /// Predicates for each pattern, in the order they appear in the query
    /// source. Captured here because `tree_sitter::Query` sorts them into
    /// separate `general`/`property` vectors and loses cross-kind order.
    predicates: Vec<Vec<InspectPredicate>>,
}

#[derive(Clone, Default)]
struct InspectPredicate {
    operator: String,
    args: Vec<InspectArg>,
}

#[derive(Clone)]
enum InspectArg {
    Capture(u32),
    String(String),
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

fn as_buffer_handle(value: &Value) -> mlua::Result<i64> {
    match value {
        Value::Integer(bufnr) => Ok(*bufnr),
        Value::Number(number) => {
            // `as` is upstream's conversion: a finite float truncates toward
            // zero (`(handle_T)lua_tointeger`, treesitter.c:575) and the cast
            // cannot trap — NaN maps to 0, magnitudes beyond `i64` saturate.
            // Whatever names no live buffer fails handle resolution with
            // `invalid buffer handle: %d`, exactly like a stale handle.
            #[expect(
                clippy::cast_possible_truncation,
                reason = "truncation is the specified `(handle_T)lua_tointeger` behavior"
            )]
            Ok(*number as i64)
        }
        _ => Err(runtime_error("expected either string or buffer handle")),
    }
}

/// The message a Rust-side failure must carry into Lua: the plain runtime
/// message, never mlua's `runtime error: ` Display prefix (upstream raises
/// bare strings through `nlua_error`).
fn error_message(error: &mlua::Error) -> String {
    match error {
        mlua::Error::RuntimeError(message) => message.clone(),
        mlua::Error::CallbackError { cause, .. } => error_message(cause),
        error => error.to_string(),
    }
}

/// Failure protocol for the string-error shims: `(false, message)` instead of
/// `Err`, because mlua pushes every `Err` a callback returns as
/// `WrappedFailure` userdata, which the `exec_lua` harness rejects with
/// "cannot be serialized over RPC". The Lua `userdata_error_shim` wrapper
/// re-raises the message as a string.
fn failure_values(lua: &Lua, error: &mlua::Error) -> mlua::Result<MultiValue> {
    Ok(MultiValue::from_iter([
        Value::Boolean(false),
        Value::String(lua.create_string(error_message(error))?),
    ]))
}

fn success_values(values: MultiValue) -> MultiValue {
    std::iter::once(Value::Boolean(true))
        .chain(values)
        .collect()
}

/// Flag-protocol combinator for `add_method` closures: converts the closure's
/// `Err` (and argument-conversion errors) into `(false, message)` values so
/// the metatable's string-error wrapper can raise them.
fn string_errors<T, A, R, M>(method: M) -> impl Fn(&Lua, &T, MultiValue) -> mlua::Result<MultiValue>
where
    A: FromLuaMulti,
    R: IntoLuaMulti,
    M: Fn(&Lua, &T, A) -> mlua::Result<R> + 'static,
{
    move |lua, this, args| match A::from_lua_multi(args, lua)
        .and_then(|args| method(lua, this, args))
    {
        Ok(result) => result.into_lua_multi(lua).map(success_values),
        Err(error) => failure_values(lua, &error),
    }
}

/// [`string_errors`] for `add_method_mut` closures.
fn string_errors_mut<T, A, R, M>(
    method: M,
) -> impl Fn(&Lua, &mut T, MultiValue) -> mlua::Result<MultiValue>
where
    A: FromLuaMulti,
    R: IntoLuaMulti,
    M: Fn(&Lua, &mut T, A) -> mlua::Result<R> + 'static,
{
    move |lua, this, args| match A::from_lua_multi(args, lua)
        .and_then(|args| method(lua, this, args))
    {
        Ok(result) => result.into_lua_multi(lua).map(success_values),
        Err(error) => failure_values(lua, &error),
    }
}

/// A plain `vim`-table function that signals failure as `(false, message)`
/// and passes through the `userdata_error_shim` wrapper, so failures reach
/// `pcall` as strings and returned userdata arrives with its metatable
/// rewired.
fn string_error_function<A, R, M>(lua: &Lua, function: M) -> mlua::Result<Function>
where
    A: FromLuaMulti,
    R: IntoLuaMulti,
    M: Fn(&Lua, A) -> mlua::Result<R> + 'static,
{
    let native = lua.create_function(move |lua, args: MultiValue| {
        match A::from_lua_multi(args, lua).and_then(|args| function(lua, args)) {
            Ok(result) => result.into_lua_multi(lua).map(success_values),
            Err(error) => failure_values(lua, &error),
        }
    })?;
    crate::vim::userdata_error_shim(lua)?.call(native)
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

fn ranges_table(
    lua: &Lua,
    ranges: impl IntoIterator<Item = Range>,
    include_bytes: bool,
) -> mlua::Result<Table> {
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
            let count = u32::try_from(parent.child_count())
                .map_err(|_| runtime_error("node path is too deep"))?;
            let index = (0..count)
                .find(|index| parent.child(*index).is_some_and(|child| child == current))
                .ok_or_else(|| runtime_error("failed to locate node in its tree"))?;
            path.push(index);
            current = parent;
        }
        path.reverse();
        Ok(Self { tree, path })
    }

    fn related(&self, node: Option<Node<'_>>) -> mlua::Result<Option<Self>> {
        node.map(|node| Self::from_node(self.tree.clone(), node))
            .transpose()
    }
}

fn add_logging_methods<M: UserDataMethods<ParserHandle>>(methods: &mut M) {
    methods.add_method_mut(
        "_set_logger",
        string_errors_mut(
            |_, this: &mut ParserHandle, (lex, parse, callback): (bool, bool, Function)| {
                check_parser_live(this)?;
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
                    let kind = match kind {
                        LogType::Lex => "lex",
                        LogType::Parse => "parse",
                    };
                    let message = message.to_owned();
                    if let Err(schedule_error) = scheduler
                        .schedule_deferred(Box::new(move || callback.call::<()>((kind, message))))
                    {
                        *error.borrow_mut() = Some(format!(
                            "treesitter logger callback scheduling failed: {schedule_error}"
                        ));
                    }
                })));
                this.logger = Some(callback);
                Ok(())
            },
        ),
    );
    methods.add_method(
        "_logger",
        string_errors(|_, this: &ParserHandle, ()| Ok(this.logger.clone())),
    );
}

/// Reads live buffer text for tree-sitter's buffer-handle `parse` input:
/// upstream parses the buffer (unsaved changes included), so the lines
/// come through `vim.api` on this same loop thread. Every line is
/// newline-terminated, including the last, unless the buffer genuinely
/// lacks a final EOL (binary, or both 'fixeol' and 'eol' off); see
/// `buffer_lacks_eol`. Sound under the `add_method_mut` borrow only
/// because `nvim_buf_get_lines` is a pure read: it fires no autocmd,
/// so the same parser cannot be reentered mid-call. Never extend this
/// helper with event-firing calls.
fn buffer_bytes(lua: &Lua, bufnr: i64) -> mlua::Result<Vec<u8>> {
    let api: Table = lua.globals().get::<Table>("vim")?.get("api")?;
    let get_lines: Function = api.get("nvim_buf_get_lines")?;
    let lines: Table = get_lines.call((bufnr, 0, -1, false))?;
    let mut bytes = Vec::new();
    for line in lines.sequence_values::<mlua::LuaString>() {
        bytes.extend_from_slice(&line?.as_bytes());
        bytes.push(b'\n');
    }
    // Upstream appends the line terminator even for the last line, and
    // drops it only when the buffer genuinely lacks one: binary, or
    // both 'fixeol' and 'eol' off (`input_cb`, treesitter.c:479-487).
    // Without the terminator tree-sitter ends every buffer root one
    // row early (`{0,0,2,1}` instead of `{0,0,3,0}` on both binaries).
    if !bytes.is_empty() && !buffer_lacks_eol(lua, &api, bufnr)? {
        return Ok(bytes);
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    Ok(bytes)
}

/// Reports whether a buffer genuinely lacks a final EOL: `eol` is off and
/// either `binary` is on or `fixeol` is off. Mirrors the last-line arm of
/// upstream `input_cb` (treesitter.c:482-483); option reads go through the same
/// `vim.api` bridge as the lines above, so no new borrow surface.
fn buffer_lacks_eol(lua: &Lua, api: &Table, bufnr: i64) -> mlua::Result<bool> {
    // `nvim_buf_get_option` is deprecated since API level 11; the
    // supported read is `nvim_get_option_value` with a `buf` scope.
    let get_option: Function = api.get("nvim_get_option_value")?;
    let scoped = |name: &str| -> mlua::Result<bool> {
        let opts = lua.create_table()?;
        opts.set("buf", bufnr)?;
        get_option.call((name, opts))
    };
    let binary = scoped("binary")?;
    let fixeol = scoped("fixeol")?;
    let eol = scoped("eol")?;
    Ok(!eol && (binary || !fixeol))
}

/// Reports whether `bufnr` names a live buffer: upstream resolves the
/// parse argument through the raw handle map (`handle_get_buffer`,
/// helpers.h:140 — no curbuf special case, unlike
/// `find_buffer_by_handle`), so `0` and negatives fail exactly like a
/// stale handle. Read through `vim.api.nvim_list_bufs` on this same
/// loop thread; a pure read like the line fetch below, so the
/// `add_method_mut` borrow stays sound.
fn buffer_handle_resolves(lua: &Lua, bufnr: i64) -> mlua::Result<bool> {
    let api: Table = lua.globals().get::<Table>("vim")?.get("api")?;
    let handles: Table = api.get::<Function>("nvim_list_bufs")?.call(())?;
    for handle in handles.sequence_values::<i64>() {
        if handle? == bufnr {
            return Ok(true);
        }
    }
    Ok(false)
}

impl UserData for ParserHandle {
    #[expect(
        clippy::too_many_lines,
        reason = "one registration closure per parser method; splitting would scatter the method table"
    )]
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_, _, ()| Ok("<parser>"));
        // Upstream exposes `__gc` through `__index`, so the finalizer is
        // callable explicitly (`parser:__gc()`); mlua reserves the real
        // `__gc` metamethod, so a regular method of the same name carries
        // it. Marking deleted is idempotent, like the real finalizer.
        methods.add_method_mut("__gc", |_, this: &mut ParserHandle, ()| {
            this.deleted = true;
            Ok(())
        });
        methods.add_method_mut(
            "reset",
            string_errors_mut(|_, this: &mut ParserHandle, ()| {
                check_parser_live(this)?;
                this.parser.reset();
                Ok(())
            }),
        );
        methods.add_method_mut(
            "set_included_ranges",
            string_errors_mut(|_, this: &mut ParserHandle, values: Table| {
                check_parser_live(this)?;
                let ranges = values
                    .sequence_values::<Value>()
                    .map(|value| value.and_then(range_from_value))
                    .collect::<mlua::Result<Vec<_>>>()?;
                this.parser
                    .set_included_ranges(&ranges)
                    .map_err(|error| runtime_error(error.to_string()))
            }),
        );
        methods.add_method(
            "included_ranges",
            string_errors(|lua, this: &ParserHandle, include_bytes: Option<bool>| {
                check_parser_live(this)?;
                ranges_table(
                    lua,
                    this.parser.included_ranges(),
                    include_bytes.unwrap_or(false),
                )
            }),
        );
        methods.add_method_mut(
            "parse",
            string_errors_mut(
                |lua,
                 this: &mut ParserHandle,
                 (old, input, include_bytes, timeout): (
                    Option<AnyUserData>,
                    Value,
                    Option<bool>,
                    Option<u64>,
                )| {
                    check_parser_live(this)?;
                    let bytes = match input {
                        Value::String(string) => string.as_bytes().to_vec(),
                        // Upstream parses live buffer text when the input is a
                        // buffer handle: fetch the lines through `vim.api` on
                        // this same loop thread (unsaved changes included) and
                        // join them exactly like the buffer store would.
                        Value::Integer(_) | Value::Number(_) => {
                            // Upstream resolves the cast value through
                            // `handle_get_buffer` before parsing
                            // (treesitter.c:575-582); a miss raises the bare
                            // `luaL_argerror` text, like the default arm.
                            let bufnr = as_buffer_handle(&input)?;
                            if !buffer_handle_resolves(lua, bufnr)? {
                                let message = format!("invalid buffer handle: {bufnr}");
                                return Err(runtime_error(message));
                            }
                            buffer_bytes(lua, bufnr)?
                        }
                        _ => return Err(runtime_error("expected either string or buffer handle")),
                    };
                    let old_tree = old
                        .as_ref()
                        .map(AnyUserData::borrow::<TreeHandle>)
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
                            if offset < length {
                                &bytes[offset..]
                            } else {
                                &[]
                            }
                        };
                        let mut progress = parse_deadline_callback(started, deadline);
                        let options = ParseOptions::new().progress_callback(&mut progress);
                        this.parser
                            .parse_with_options(&mut input, old_tree_ref, Some(options))
                    }
                    .ok_or_else(|| {
                        runtime_error(
                            "Language was unset, has an incompatible ABI, or parsing timed out.",
                        )
                    })?;
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
                        language: this.language.clone(),
                    }));
                    Ok((
                        tree,
                        ranges_table(lua, changed, include_bytes.unwrap_or(false))?,
                    ))
                },
            ),
        );
        add_logging_methods(methods);
    }
}

impl UserData for TreeHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_, _, ()| Ok("<tree>"));
        methods.add_meta_method(MetaMethod::Eq, |_, this, other: AnyUserData| {
            let other = other.borrow::<TreeHandle>()?;
            Ok(Arc::ptr_eq(&this.0, &other.0))
        });
        methods.add_method(
            "copy",
            string_errors(|_, this: &TreeHandle, ()| {
                Ok(TreeHandle(Arc::new(TreeData {
                    tree: this.0.tree.clone(),
                    source: this.0.source.clone(),
                    language: this.0.language.clone(),
                })))
            }),
        );
        methods.add_method(
            "root",
            string_errors(|_, this: &TreeHandle, ()| {
                Ok(NodeHandle {
                    tree: this.clone(),
                    path: Vec::new(),
                })
            }),
        );
        methods.add_method(
            "included_ranges",
            string_errors(|lua, this: &TreeHandle, include_bytes: Option<bool>| {
                ranges_table(
                    lua,
                    this.0.tree.included_ranges(),
                    include_bytes.unwrap_or(false),
                )
            }),
        );
        methods.add_method(
            "edit",
            string_errors(|_, this: &TreeHandle, args: Variadic<i64>| {
                if args.len() != 9 {
                    return Err(runtime_error("not enough args to tree:edit()"));
                }
                let mut tree = this.0.tree.clone();
                tree.edit(&InputEdit {
                    start_byte: usize::try_from(args[0])
                        .map_err(|_| runtime_error("start byte out of bounds"))?,
                    old_end_byte: usize::try_from(args[1])
                        .map_err(|_| runtime_error("old end byte out of bounds"))?,
                    new_end_byte: usize::try_from(args[2])
                        .map_err(|_| runtime_error("new end byte out of bounds"))?,
                    start_position: point(args[3], args[4])?,
                    old_end_position: point(args[5], args[6])?,
                    new_end_position: point(args[7], args[8])?,
                });
                Ok(TreeHandle(Arc::new(TreeData {
                    tree,
                    source: this.0.source.clone(),
                    language: this.0.language.clone(),
                })))
            }),
        );
    }
}

fn push_optional_node(value: Option<NodeHandle>) -> Option<NodeHandle> {
    value
}

#[expect(
    clippy::too_many_lines,
    reason = "node navigation methods form one ordered UserData registration group; splitting would scatter the traversal API"
)]
fn add_navigation_methods<M: UserDataMethods<NodeHandle>>(methods: &mut M) {
    methods.add_method(
        "child",
        string_errors(|_, this: &NodeHandle, index: i64| {
            let index = checked_u32(index, "child index")?;
            let node = this.resolve()?.child(index);
            Ok(push_optional_node(this.related(node)?))
        }),
    );
    methods.add_method(
        "named_child",
        string_errors(|_, this: &NodeHandle, index: i64| {
            let index = checked_u32(index, "child index")?;
            let node = this.resolve()?.named_child(index);
            Ok(push_optional_node(this.related(node)?))
        }),
    );
    methods.add_method(
        "parent",
        string_errors(|_, this: &NodeHandle, ()| {
            let node = this.resolve()?.parent();
            Ok(push_optional_node(this.related(node)?))
        }),
    );
    methods.add_method(
        "next_sibling",
        string_errors(|_, this: &NodeHandle, ()| {
            let node = this.resolve()?.next_sibling();
            Ok(push_optional_node(this.related(node)?))
        }),
    );
    methods.add_method(
        "prev_sibling",
        string_errors(|_, this: &NodeHandle, ()| {
            let node = this.resolve()?.prev_sibling();
            Ok(push_optional_node(this.related(node)?))
        }),
    );
    methods.add_method(
        "next_named_sibling",
        string_errors(|_, this: &NodeHandle, ()| {
            let node = this.resolve()?.next_named_sibling();
            Ok(push_optional_node(this.related(node)?))
        }),
    );
    methods.add_method(
        "prev_named_sibling",
        string_errors(|_, this: &NodeHandle, ()| {
            let node = this.resolve()?.prev_named_sibling();
            Ok(push_optional_node(this.related(node)?))
        }),
    );
    methods.add_method(
        "descendant_for_range",
        string_errors(
            |_, this: &NodeHandle, (sr, sc, er, ec): (i64, i64, i64, i64)| {
                let node = this
                    .resolve()?
                    .descendant_for_point_range(point(sr, sc)?, point(er, ec)?);
                Ok(push_optional_node(this.related(node)?))
            },
        ),
    );
    methods.add_method(
        "named_descendant_for_range",
        string_errors(
            |_, this: &NodeHandle, (sr, sc, er, ec): (i64, i64, i64, i64)| {
                let node = this
                    .resolve()?
                    .named_descendant_for_point_range(point(sr, sc)?, point(er, ec)?);
                Ok(push_optional_node(this.related(node)?))
            },
        ),
    );
    methods.add_method(
        "child_with_descendant",
        string_errors(|_, this: &NodeHandle, descendant: AnyUserData| {
            let descendant = descendant.borrow::<NodeHandle>()?;
            if !Arc::ptr_eq(&this.tree.0, &descendant.tree.0) {
                return Ok(None);
            }
            let node = this.resolve()?.child_with_descendant(descendant.resolve()?);
            this.related(node)
        }),
    );
    methods.add_method(
        "field",
        string_errors(|_, this: &NodeHandle, name: String| {
            let node = this.resolve()?;
            let mut result = Vec::new();
            for index in 0..node.child_count() {
                let index =
                    u32::try_from(index).map_err(|_| runtime_error("child index out of bounds"))?;
                if node.field_name_for_child(index) == Some(name.as_str())
                    && let Some(child) = node.child(index)
                {
                    result.push(NodeHandle::from_node(this.tree.clone(), child)?);
                }
            }
            Ok(result)
        }),
    );
    methods.add_method(
        "named_children",
        string_errors(|_, this: &NodeHandle, ()| {
            let node = this.resolve()?;
            let mut result = Vec::new();
            for index in 0..node.named_child_count() {
                let index =
                    u32::try_from(index).map_err(|_| runtime_error("child index out of bounds"))?;
                if let Some(child) = node.named_child(index) {
                    result.push(NodeHandle::from_node(this.tree.clone(), child)?);
                }
            }
            Ok(result)
        }),
    );
}

fn add_geometry_methods<M: UserDataMethods<NodeHandle>>(methods: &mut M) {
    methods.add_method(
        "range",
        string_errors(|lua, this: &NodeHandle, include_bytes: Option<bool>| {
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
        }),
    );
    methods.add_method(
        "start",
        string_errors(|_, this: &NodeHandle, ()| {
            let n = this.resolve()?;
            let p = n.start_position();
            Ok((p.row, p.column, n.start_byte()))
        }),
    );
    methods.add_method(
        "end_",
        string_errors(|_, this: &NodeHandle, ()| {
            let n = this.resolve()?;
            let p = n.end_position();
            Ok((p.row, p.column, n.end_byte()))
        }),
    );
}

impl UserData for NodeHandle {
    #[expect(
        clippy::too_many_lines,
        reason = "node methods must register together on one userdata builder; the closures share the resolve/related helpers"
    )]
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            Ok(format!("<node {}>", this.resolve()?.kind()))
        });
        methods.add_meta_method(MetaMethod::Eq, |_, this, other: AnyUserData| {
            let other = other.borrow::<NodeHandle>()?;
            Ok(Arc::ptr_eq(&this.tree.0, &other.tree.0) && this.path == other.path)
        });
        methods.add_meta_method(MetaMethod::Len, |_, this, ()| {
            Ok(this.resolve()?.child_count())
        });
        methods.add_method(
            "id",
            string_errors(|lua, this: &NodeHandle, ()| {
                lua.create_string(this.resolve()?.id().to_ne_bytes())
            }),
        );
        add_geometry_methods(methods);
        methods.add_method(
            "type",
            string_errors(|_, this: &NodeHandle, ()| Ok(this.resolve()?.kind().to_owned())),
        );
        methods.add_method(
            "symbol",
            string_errors(|_, this: &NodeHandle, ()| Ok(this.resolve()?.kind_id())),
        );
        methods.add_method(
            "named",
            string_errors(|_, this: &NodeHandle, ()| Ok(this.resolve()?.is_named())),
        );
        methods.add_method(
            "missing",
            string_errors(|_, this: &NodeHandle, ()| Ok(this.resolve()?.is_missing())),
        );
        methods.add_method(
            "extra",
            string_errors(|_, this: &NodeHandle, ()| Ok(this.resolve()?.is_extra())),
        );
        methods.add_method(
            "has_changes",
            string_errors(|_, this: &NodeHandle, ()| Ok(this.resolve()?.has_changes())),
        );
        methods.add_method(
            "has_error",
            string_errors(|_, this: &NodeHandle, ()| Ok(this.resolve()?.has_error())),
        );
        methods.add_method(
            "sexpr",
            string_errors(|_, this: &NodeHandle, ()| Ok(this.resolve()?.to_sexp())),
        );
        methods.add_method(
            "child_count",
            string_errors(|_, this: &NodeHandle, ()| Ok(this.resolve()?.child_count())),
        );
        methods.add_method(
            "named_child_count",
            string_errors(|_, this: &NodeHandle, ()| Ok(this.resolve()?.named_child_count())),
        );
        methods.add_method(
            "byte_length",
            string_errors(|_, this: &NodeHandle, ()| {
                let n = this.resolve()?;
                Ok(n.end_byte() - n.start_byte())
            }),
        );
        methods.add_method(
            "tree",
            string_errors(|_, this: &NodeHandle, ()| Ok(this.tree.clone())),
        );
        methods.add_method(
            "root",
            string_errors(|_, this: &NodeHandle, ()| {
                Ok(NodeHandle {
                    tree: this.tree.clone(),
                    path: Vec::new(),
                })
            }),
        );
        methods.add_method(
            "equal",
            string_errors(|_, this: &NodeHandle, other: AnyUserData| {
                let other = other.borrow::<NodeHandle>()?;
                Ok(Arc::ptr_eq(&this.tree.0, &other.tree.0) && this.path == other.path)
            }),
        );
        add_navigation_methods(methods);
        methods.add_method(
            "iter_children",
            string_errors(|lua, this: &NodeHandle, ()| {
                let source = this.clone();
                let index = Rc::new(Cell::new(0u32));
                string_error_function(lua, move |_, ()| {
                    let current = index.get();
                    let node = source.resolve()?;
                    let Some(child) = node.child(current) else {
                        return Ok((None, None));
                    };
                    index.set(current.saturating_add(1));
                    let field = node.field_name_for_child(current).map(str::to_owned);
                    Ok((
                        Some(NodeHandle::from_node(source.tree.clone(), child)?),
                        field,
                    ))
                })
            }),
        );
        methods.add_method(
            "__has_ancestor",
            string_errors(|_, this: &NodeHandle, predicate: Table| {
                let types = predicate
                    .sequence_values::<String>()
                    .skip(2)
                    .collect::<mlua::Result<HashSet<_>>>()?;
                let mut node = this.resolve()?;
                while let Some(parent) = node.parent() {
                    if types.contains(parent.kind()) {
                        return Ok(true);
                    }
                    node = parent;
                }
                Ok(false)
            }),
        );
    }
}


impl UserData for QueryHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_, _, ()| Ok("<query>"));
        methods.add_method_mut(
            "disable_capture",
            string_errors_mut(|_, this: &mut QueryHandle, name: String| {
                this.query.disable_capture(&name);
                Ok(())
            }),
        );
        methods.add_method_mut(
            "disable_pattern",
            string_errors_mut(|_, this: &mut QueryHandle, index: i64| {
                let index = usize::try_from(index)
                    .map_err(|_| runtime_error("pattern index out of bounds"))?;
                if index == 0 || index > this.query.pattern_count() {
                    return Err(runtime_error("pattern index out of bounds"));
                }
                this.query.disable_pattern(index - 1);
                Ok(())
            }),
        );
        methods.add_method(
            "inspect",
            string_errors(|lua, this: &QueryHandle, ()| {
                let result = lua.create_table()?;
                let captures = lua.create_table()?;
                for (index, name) in this.query.capture_names().iter().enumerate() {
                    captures.raw_set(index + 1, *name)?;
                }
                result.set("captures", captures)?;
                let patterns = lua.create_table()?;
                for (index, predicates) in this.predicates.iter().enumerate() {
                    let predicates_table = lua.create_table()?;
                    for (slot, predicate) in predicates.iter().enumerate() {
                        let values = lua.create_table()?;
                        values.raw_set(1, predicate.operator.as_str())?;
                        for (arg_index, arg) in predicate.args.iter().enumerate() {
                            match arg {
                                InspectArg::Capture(id) => {
                                    values.raw_set(arg_index + 2, *id + 1)?;
                                }
                                InspectArg::String(s) => {
                                    values.raw_set(arg_index + 2, s.as_str())?;
                                }
                            }
                        }
                        predicates_table.raw_set(slot + 1, values)?;
                    }
                    patterns.raw_set(index + 1, predicates_table)?;
                }
                result.set("patterns", patterns)?;
                Ok(result)
            }),
        );
    }
}

impl UserData for MatchHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method(
            "info",
            string_errors(|_, this: &MatchHandle, ()| Ok((this.id, this.pattern_index + 1))),
        );
        methods.add_method(
            "captures",
            string_errors(|lua, this: &MatchHandle, ()| {
                let result = lua.create_table()?;
                for (capture, node) in &this.captures {
                    let index = usize::try_from(*capture)
                        .map_err(|_| runtime_error("capture id out of bounds"))?
                        + 1;
                    let nodes = match result.raw_get::<Value>(index)? {
                        Value::Table(table) => table,
                        _ => lua.create_table()?,
                    };
                    nodes.raw_set(nodes.raw_len() + 1, node.clone())?;
                    result.raw_set(index, nodes)?;
                }
                Ok(result)
            }),
        );
    }
}

impl UserData for CursorHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut(
            "remove_match",
            string_errors_mut(|_, this: &mut CursorHandle, id: i64| {
                this.removed.insert(checked_u32(id, "match id")?);
                Ok(())
            }),
        );
        methods.add_method_mut(
            "next_match",
            string_errors_mut(|_, this: &mut CursorHandle, ()| {
                while let Some(value) = this.matches.get(this.next_match).cloned() {
                    this.next_match += 1;
                    if !this.removed.contains(&value.id) {
                        return Ok(Some(value));
                    }
                }
                Ok(None)
            }),
        );
        methods.add_method_mut(
            "next_capture",
            string_errors_mut(|_, this: &mut CursorHandle, ()| {
                while let Some((index, node, matched)) =
                    this.captures.get(this.next_capture).cloned()
                {
                    this.next_capture += 1;
                    if !this.removed.contains(&matched.id) {
                        return Ok((Some(index + 1), Some(node), Some(matched)));
                    }
                }
                Ok((None, None, None))
            }),
        );
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
        let library = Library::new(Path::new(path))
            .map_err(|error| runtime_error(format!("Failed to load parser: {error}")))?;
        let function = library
            .get::<unsafe extern "C" fn() -> *const ()>(symbol_name.as_bytes())
            .map_err(|error| runtime_error(format!("Failed to load parser: {error}")))?;
        let language = Language::new(LanguageFn::from_raw(*function));
        (library, language)
    };
    let version = language.abi_version();
    if !(tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION..=tree_sitter::LANGUAGE_VERSION)
        .contains(&version)
    {
        return Err(runtime_error(format!(
            "ABI version mismatch for {path}: supported between {} and {}, found {version}",
            tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION,
            tree_sitter::LANGUAGE_VERSION,
        )));
    }
    Ok(LoadedLanguage {
        language,
        _library: library,
    })
}

fn registry_language(languages: &Languages, name: &str) -> mlua::Result<Arc<LoadedLanguage>> {
    languages
        .borrow()
        .get(name)
        .cloned()
        .ok_or_else(|| runtime_error(format!("no such language: {name}")))
}

fn inspect_language(lua: &Lua, language: &Language) -> mlua::Result<Table> {
    let result = lua.create_table()?;
    let symbols = lua.create_table()?;
    for id in 0..language.node_kind_count() {
        let id = u16::try_from(id).map_err(|_| runtime_error("node kind id out of bounds"))?;
        if let Some(kind) = language.node_kind_for_id(id) {
            let key = if language.node_kind_is_named(id) {
                kind.to_owned()
            } else {
                format!("\"{kind}\"")
            };
            symbols.set(key, language.node_kind_is_named(id))?;
        }
    }
    result.set("symbols", symbols)?;
    let fields = lua.create_table()?;
    for id in 1..=language.field_count() {
        let id = u16::try_from(id).map_err(|_| runtime_error("field id out of bounds"))?;
        if let Some(field) = language.field_name_for_id(id) {
            fields.raw_set(id, field)?;
        }
    }
    result.set("fields", fields)?;
    result.set("_wasm", false)?;
    result.set("abi_version", language.abi_version())?;
    result.set("state_count", language.parse_state_count())?;
    if let Some(metadata) = language.metadata() {
        let table = lua.create_table()?;
        table.set("major_version", metadata.major_version)?;
        table.set("minor_version", metadata.minor_version)?;
        table.set("patch_version", metadata.patch_version)?;
        result.set("metadata", table)?;
    }
    let supertypes = lua.create_table()?;
    for &supertype in language.supertypes() {
        let children = lua.create_table()?;
        for (index, &subtype) in language
            .subtypes_for_supertype(supertype)
            .iter()
            .enumerate()
        {
            if let Some(kind) = language.node_kind_for_id(subtype) {
                children.raw_set(index + 1, kind)?;
            }
        }
        if let Some(kind) = language.node_kind_for_id(supertype) {
            supertypes.set(kind, children)?;
        }
    }
    result.set("supertypes", supertypes)?;
    Ok(result)
}

/// Reads a query-cursor row/column bound, tolerating the conventional `-1`
/// ("unbounded", e.g. `iter_matches(root, 0, 0, -1)`). Upstream casts the
/// Lua integer to `uint32_t`, so `-1` wraps to the maximum; saturating here
/// reaches the same bound without a wrapping cast.
fn cursor_bound(value: Option<i64>, default: usize) -> usize {
    value.map_or(default, |bound| {
        usize::try_from(bound).unwrap_or(usize::MAX)
    })
}

fn configure_cursor(cursor: &mut QueryCursor, options: &Table) -> mlua::Result<()> {
    let start = Point::new(
        cursor_bound(options.get("start_row")?, 0),
        cursor_bound(options.get("start_col")?, 0),
    );
    let end = Point::new(
        cursor_bound(options.get("end_row")?, usize::MAX),
        cursor_bound(options.get("end_col")?, usize::MAX),
    );
    cursor.set_point_range(start..end);
    if let Some(limit) = options.get::<Option<u32>>("match_limit")? {
        cursor.set_match_limit(limit);
    }
    if let Some(depth) = options.get::<Option<u32>>("max_start_depth")? {
        cursor.set_max_start_depth(Some(depth));
    }
    Ok(())
}

fn collect_query_cursor(
    node: &NodeHandle,
    query: &QueryHandle,
    options: Option<&Table>,
) -> mlua::Result<CursorHandle> {
    let resolved = node.resolve()?;
    let mut cursor = QueryCursor::new();
    if let Some(options) = options {
        configure_cursor(&mut cursor, options)?;
    }
    let mut matches = Vec::new();
    let mut iterator = cursor.matches(&query.query, resolved, node.tree.0.source.as_ref());
    while let Some(matched) = iterator.next() {
        let captures = matched
            .captures
            .iter()
            .map(|capture| {
                Ok((
                    capture.index,
                    NodeHandle::from_node(node.tree.clone(), capture.node)?,
                ))
            })
            .collect::<mlua::Result<Vec<_>>>()?;
        matches.push(MatchHandle {
            id: matched.id(),
            pattern_index: matched.pattern_index,
            captures,
        });
    }
    let mut capture_cursor = QueryCursor::new();
    if let Some(options) = options {
        configure_cursor(&mut capture_cursor, options)?;
    }
    let mut captures = Vec::new();
    let mut iterator = capture_cursor.captures(&query.query, resolved, node.tree.0.source.as_ref());
    while let Some((matched, capture_index)) = iterator.next() {
        let all = matched
            .captures
            .iter()
            .map(|capture| {
                Ok((
                    capture.index,
                    NodeHandle::from_node(node.tree.clone(), capture.node)?,
                ))
            })
            .collect::<mlua::Result<Vec<_>>>()?;
        let capture = matched.captures[*capture_index];
        captures.push((
            capture.index,
            NodeHandle::from_node(node.tree.clone(), capture.node)?,
            MatchHandle {
                id: matched.id(),
                pattern_index: matched.pattern_index,
                captures: all,
            },
        ));
    }
    Ok(CursorHandle {
        matches,
        captures,
        next_match: 0,
        next_capture: 0,
        removed: HashSet::new(),
    })
}

/// Walks every capture `query` produces over `node`'s subtree and writes it
/// as a persistent (non-ephemeral) highlight extmark through
/// `vim.api.nvim_buf_set_extmark`, so the result lands in the buffer's real
/// extmark store — the only source `crates/ox-ui/src/compositor.rs`
/// `apply_extmark_highlights` paints from. Upstream drives this from
/// ephemeral, redraw-triggered `on_range`/`on_win` decoration-provider
/// callbacks (`runtime/lua/vim/treesitter/highlighter.lua:343-503`,
/// `469-479` for the `nvim_buf_set_extmark(..., ephemeral = true, ...)`
/// call); this port's compositor never dispatches decoration providers
/// during redraw (`nvim_set_decoration_provider`,
/// `crates/ox-api/src/extmark.rs:764-786`, stores callbacks but nothing
/// invokes them), so this emits real, painted extmarks instead of relying
/// on that path.
///
/// Group naming mirrors `TSHighlighterQuery:get_hl_from_capture`
/// (`highlighter.lua:38-49`): a capture named `_foo` (or empty) carries no
/// highlight; otherwise the group is `"@" .. name` (the bare capture name).
/// Oxvim's `HlState::group_id` (`crates/ox-ui/src/hl.rs:394`) is an exact-match
/// lookup with no dotted-suffix fallback, and the default `colors/vim.lua`
/// registers only bare `@name` links (not `@name.lang`), so emitting the bare
/// name is the form that resolves to color here.
/// Priority mirrors the same file's `on_range_impl` (`highlighter.lua:456`:
/// `local priority = (tonumber(metadata.priority) or metadata[capture] and
/// metadata[capture].priority) or vim.hl.priorities.treesitter`): a
/// `(#set! priority N)` directive on the pattern wins, otherwise the
/// default treesitter priority is 100 (`runtime/lua/vim/hl.lua`
/// `priorities.treesitter`).
///
/// Standard predicates (`#eq?`, `#not-eq?`, `#any-eq?`, `#match?`,
/// `#not-match?`, `#any-of?`, `#not-any-of?`) are already evaluated by the
/// tree-sitter engine itself inside `QueryCursor::captures`/`::matches`
/// (`tree-sitter` 0.26.12 `binding_rust/lib.rs:3416-3467`,
/// `satisfies_text_predicates`), the same mechanism [`collect_query_cursor`]
/// already relies on, so no predicate re-evaluation happens here. Custom
/// Lua-registered predicates (`vim.treesitter.query.add_predicate`, e.g.
/// `#has-ancestor?`) are a Lua-only concept upstream and are out of scope:
/// their `general_predicates` entries are not consulted, so a pattern that
/// depends on one highlights unconditionally rather than being filtered.
fn emit_highlight_extmarks(
    lua: &Lua,
    node: &NodeHandle,
    query: &QueryHandle,
    bufnr: i64,
    ns: i64,
) -> mlua::Result<()> {
    let vim: Table = lua.globals().get("vim")?;
    let api: Table = vim.get("api")?;
    let set_extmark: Function = api.get("nvim_buf_set_extmark")?;

    let resolved = node.resolve()?;
    let mut cursor = QueryCursor::new();
    let mut iterator = cursor.captures(&query.query, resolved, node.tree.0.source.as_ref());
    while let Some((matched, capture_index)) = iterator.next() {
        let capture = matched.captures[*capture_index];
        let capture_slot = usize::try_from(capture.index)
            .map_err(|_| runtime_error("capture index out of bounds"))?;
        let Some(name) = query.query.capture_names().get(capture_slot).copied() else {
            continue;
        };
        if name.is_empty() || name.starts_with('_') {
            continue;
        }
        let priority = query
            .query
            .property_settings(matched.pattern_index)
            .iter()
            .find(|property| &*property.key == "priority")
            .and_then(|property| property.value.as_deref())
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(100);
        let range = capture.node.range();
        let start_row =
            i64::try_from(range.start_point.row).map_err(|_| runtime_error("row out of bounds"))?;
        let start_col = i64::try_from(range.start_point.column)
            .map_err(|_| runtime_error("column out of bounds"))?;
        let end_row =
            i64::try_from(range.end_point.row).map_err(|_| runtime_error("row out of bounds"))?;
        let end_col = i64::try_from(range.end_point.column)
            .map_err(|_| runtime_error("column out of bounds"))?;
        let opts = lua.create_table()?;
        opts.set("hl_group", format!("@{name}"))?;
        opts.set("end_row", end_row)?;
        opts.set("end_col", end_col)?;
        opts.set("priority", priority)?;
        opts.set("strict", false)?;
        set_extmark.call::<i64>((bufnr, ns, start_row, start_col, opts))?;
    }
    Ok(())
}

/// Install Neovim's tree-sitter C-facing fields on the existing `vim` table.
pub(crate) fn install(lua: &Lua, scheduler: Rc<dyn Scheduler>) -> mlua::Result<()> {
    let vim: Table = lua.globals().get("vim")?;
    let languages: Languages = Rc::new(RefCell::new(HashMap::new()));

    let registry = languages.clone();
    vim.set(
        "_ts_add_language_from_object",
        string_error_function(
            lua,
            move |_, (path, name, symbol): (String, String, Option<String>)| {
                if registry.borrow().contains_key(&name) {
                    return Ok(true);
                }
                let loaded = Arc::new(load_language(&path, symbol.as_deref().unwrap_or(&name))?);
                registry.borrow_mut().insert(name, loaded);
                Ok(true)
            },
        )?,
    )?;
    let registry = languages.clone();
    vim.set(
        "_ts_has_language",
        lua.create_function(move |_, name: String| Ok(registry.borrow().contains_key(&name)))?,
    )?;
    let registry = languages.clone();
    vim.set(
        "_ts_remove_language",
        lua.create_function(move |_, name: String| {
            Ok(registry.borrow_mut().remove(&name).is_some())
        })?,
    )?;

    let registry = languages.clone();
    vim.set(
        "_create_ts_parser",
        string_error_function(lua, move |_, name: String| {
            let language = registry_language(&registry, &name)?;
            let mut parser = Parser::new();
            parser.set_language(&language.language).map_err(|error| {
                runtime_error(format!("Failed to load language : {name}: {error}"))
            })?;
            Ok(ParserHandle {
                parser,
                language,
                scheduler: scheduler.clone(),
                logger: None,
                logger_error: Rc::new(RefCell::new(None)),
                deleted: false,
            })
        })?,
    )?;

/// Walks the raw predicate steps for a compiled tree-sitter query and returns
/// them in source order. The safe `Query` API splits predicates into separate
/// `general`/`property`/`text` vectors, so this is the only way to recover the
/// interleaved order used by upstream `query_inspect`.
///
/// # Safety
///
/// `query` must be a valid, non-null `TSQuery` pointer returned by
/// `ts_query_new` (or `Query::new_raw`). It must outlive this function call.
unsafe fn parse_query_predicates(
    query: *const tree_sitter::ffi::TSQuery,
    pattern_count: usize,
) -> mlua::Result<Vec<Vec<InspectPredicate>>> {
    let mut predicates = Vec::with_capacity(pattern_count);
    for pattern_index in 0..pattern_count {
        predicates.push(unsafe { parse_pattern_predicates(query, pattern_index)? });
    }
    Ok(predicates)
}

/// # Safety
///
/// `query` must be a valid, non-null `TSQuery` pointer that outlives this call.
unsafe fn parse_pattern_predicates(
    query: *const tree_sitter::ffi::TSQuery,
    pattern_index: usize,
) -> mlua::Result<Vec<InspectPredicate>> {
    let mut length = 0u32;
    // SAFETY: `query` is a valid TSQuery pointer by the caller's contract.
    let steps = unsafe { tree_sitter::ffi::ts_query_predicates_for_pattern(
        query,
        pattern_index as u32,
        &mut length,
    ) };
    if length == 0 {
        return Ok(Vec::new());
    }
    // SAFETY: `ts_query_predicates_for_pattern` returned `length` valid steps.
    let steps = unsafe { std::slice::from_raw_parts(steps, length as usize) };
    let mut pattern_predicates = Vec::new();
    let mut current = InspectPredicate {
        operator: String::new(),
        args: Vec::new(),
    };
    let mut has_operator = false;
    for step in steps {
        if step.type_ == tree_sitter::ffi::TSQueryPredicateStepTypeDone {
            if has_operator {
                pattern_predicates.push(std::mem::take(&mut current));
                has_operator = false;
            }
        } else if step.type_ == tree_sitter::ffi::TSQueryPredicateStepTypeString {
            // SAFETY: `query` is valid and `value_id` is a string from it.
            let s = unsafe { query_string_value(query, step.value_id)? };
            if !has_operator {
                current.operator = s;
                has_operator = true;
            } else {
                current.args.push(InspectArg::String(s));
            }
        } else if step.type_ == tree_sitter::ffi::TSQueryPredicateStepTypeCapture && has_operator {
            current.args.push(InspectArg::Capture(step.value_id));
        }
    }
    Ok(pattern_predicates)
}

/// # Safety
///
/// `query` must be a valid, non-null `TSQuery` pointer that outlives this call,
/// and `id` must be a valid string value id in that query.
unsafe fn query_string_value(
    query: *const tree_sitter::ffi::TSQuery,
    id: u32,
) -> mlua::Result<String> {
    let mut length = 0u32;
    // SAFETY: `query` is valid and `id` is a string value id by the caller.
    let ptr = unsafe { tree_sitter::ffi::ts_query_string_value_for_id(query, id, &mut length) };
    // SAFETY: `ts_query_string_value_for_id` returned `length` bytes from the query.
    let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), length as usize) };
    std::str::from_utf8(bytes)
        .map(|s| s.to_string())
        .map_err(|_| runtime_error("query string value is not valid UTF-8"))
}

    let registry = languages.clone();
    vim.set(
        "_ts_parse_query",
        string_error_function(lua, move |_, (name, source): (String, String)| {
            let language = registry_language(&registry, &name)?;
            let query = Query::new(&language.language, &source)
                .map_err(|error| runtime_error(error.to_string()))?;
            let raw = Query::new_raw(&language.language, &source)
                .map_err(|error| runtime_error(error.to_string()))?;
            let predicates = unsafe { parse_query_predicates(raw, query.pattern_count()) };
            unsafe { tree_sitter::ffi::ts_query_delete(raw) };
            let predicates = predicates.map_err(|error| runtime_error(error.to_string()))?;
            Ok(QueryHandle {
                query,
                _language: language,
                predicates,
            })
        })?,
    )?;
    let registry = languages.clone();
    vim.set(
        "_ts_inspect_language",
        string_error_function(lua, move |lua, name: String| {
            let language = registry_language(&registry, &name)?;
            inspect_language(lua, &language.language)
        })?,
    )?;

    vim.set(
        "_create_ts_querycursor",
        string_error_function(
            lua,
            move |_, (node, query, options): (AnyUserData, AnyUserData, Option<Table>)| {
                let node = node.borrow::<NodeHandle>()?;
                let query = query.borrow::<QueryHandle>()?;
                collect_query_cursor(&node, &query, options.as_ref())
            },
        )?,
    )?;

    vim.set(
        "_ts_emit_highlights",
        string_error_function(
            lua,
            move |lua, (node, query, bufnr, ns): (AnyUserData, AnyUserData, i64, i64)| {
                let node = node.borrow::<NodeHandle>()?;
                let query = query.borrow::<QueryHandle>()?;
                emit_highlight_extmarks(lua, &node, &query, bufnr, ns)
            },
        )?,
    )?;

    vim.set(
        "_ts_get_language_version",
        lua.create_function(|_, ()| Ok(tree_sitter::LANGUAGE_VERSION))?,
    )?;
    vim.set(
        "_ts_get_minimum_language_version",
        lua.create_function(|_, ()| Ok(tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION))?,
    )?;
    Ok(())
}
