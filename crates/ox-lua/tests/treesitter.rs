//! Integration coverage for the real tree-sitter dynamic-parser boundary.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use ox_lua::{BuiltinHost, LuaHost, RuntimeRoot, Scheduler, Work};
use ox_types::{OxStr, Typval};

#[derive(Default)]
struct TestScheduler {
    queue: RefCell<VecDeque<Work>>,
}

impl TestScheduler {
    fn drain(&self) -> mlua::Result<()> {
        while let Some(work) = self.queue.borrow_mut().pop_front() {
            work()?;
        }
        Ok(())
    }
}

impl Scheduler for TestScheduler {
    fn schedule_deferred(&self, work: Work) -> Result<(), String> {
        self.queue.borrow_mut().push_back(work);
        Ok(())
    }
}

struct NoBuiltins;

impl BuiltinHost for NoBuiltins {
    fn call(&self, name: &OxStr, _args: Vec<Typval>) -> Result<Typval, String> {
        // The runtime prelude probes has('win32') during host init
        // (runtime/lua/vim/_core/system.lua).
        if name.as_bytes() == b"has" {
            return Ok(Typval::Number(0));
        }
        Err(format!(
            "unexpected Vimscript builtin call: {}",
            name.to_string_lossy()
        ))
    }
}

fn parser_from_environment() -> Option<(PathBuf, String)> {
    if let Some(path) = std::env::var_os("OXVIM_TREE_SITTER_PARSER").map(PathBuf::from) {
        let language = std::env::var("OXVIM_TREE_SITTER_LANGUAGE")
            .ok()
            .or_else(|| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(str::to_owned)
            })?;
        return path.is_file().then_some((path, language));
    }

    let root = std::env::var_os("OXVIM_REF_ROOT").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.references/neovim"),
        PathBuf::from,
    );
    [
        root.join("build/lib/nvim/parser/lua.so"),
        root.join(".deps/usr/lib/nvim/parser/lua.so"),
        root.join("build/lib/nvim/parser/c.so"),
        root.join(".deps/usr/lib/nvim/parser/c.so"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .map(|path| {
        let language = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("lua")
            .to_owned();
        (path, language)
    })
}

fn runtime_root() -> RuntimeRoot {
    RuntimeRoot::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime"))
}

/// These tests pin the real parser boundary: a missing parser shared
/// object fails loudly instead of reporting a green suite that ran
/// nothing.
fn require_parser() -> (PathBuf, String) {
    parser_from_environment().unwrap_or_else(|| {
        panic!(
            "treesitter integration test needs a parser .so: set OXVIM_TREE_SITTER_PARSER              (+ OXVIM_TREE_SITTER_LANGUAGE) or OXVIM_REF_ROOT to a built Neovim checkout"
        )
    })
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one stateful Tree-sitter scenario exercises parse, node, edit, query, and lifetime boundaries through a single Lua chunk"
)]
fn real_parser_exercises_parse_nodes_edit_queries_and_lifetimes() {
    let (parser, language) = require_parser();

    let scheduler = Rc::new(TestScheduler::default());
    let host = LuaHost::new(runtime_root(), Rc::new(NoBuiltins), scheduler.clone()).unwrap();
    let lua = host.lua();
    lua.globals()
        .set("parser_path", parser.to_string_lossy().as_ref())
        .unwrap();
    lua.globals().set("parser_language", language).unwrap();

    let result: mlua::Table = lua
        .load(
            r"
            assert(vim._ts_add_language_from_object(parser_path, parser_language))
            assert(vim._ts_has_language(parser_language))
            assert(vim._ts_get_minimum_language_version() <= vim._ts_get_language_version())

            local inspected = vim._ts_inspect_language(parser_language)
            assert(type(inspected.symbols) == 'table')
            assert(type(inspected.fields) == 'table')
            assert(type(inspected.abi_version) == 'number')

            local parser = vim._create_ts_parser(parser_language)
            local logs = 0
            parser:_set_logger(true, true, function(kind, message)
              assert(kind == 'lex' or kind == 'parse')
              assert(type(message) == 'string')
              logs = logs + 1
            end)
            assert(type(parser:_logger()) == 'function')

            local source = 'local value = 1\n'
            parser:set_included_ranges({ { 0, 0, 0, 1, 0, #source } })
            local configured_ranges = parser:included_ranges(true)
            assert(#configured_ranges == 1)
            assert(configured_ranges[1][1] == 0 and configured_ranges[1][3] == 0)
            assert(configured_ranges[1][4] == 1 and configured_ranges[1][6] == #source)

            local tree, initial_ranges = parser:parse(nil, source, true)
            assert(type(initial_ranges) == 'table' and #initial_ranges > 0)
            local root = tree:root()
            local sr, sc, sb, er, ec, eb = root:range(true)
            assert(sr == 0 and sc == 0 and sb == 0 and eb == #source)
            assert(er >= sr and ec >= 0)
            assert(root:start() == 0)
            assert(root:end_() >= 0)
            assert(root:tree() == tree)
            assert(root:root():equal(root))
            assert(root:byte_length() == eb - sb)
            assert(type(root:sexpr()) == 'string')
            assert(type(root:type()) == 'string')
            assert(type(root:symbol()) == 'number')
            assert(type(root:named_children()) == 'table')

            local iterated = 0
            for child, field in root:iter_children() do
              assert(child:parent():equal(root))
              assert(field == nil or type(field) == 'string')
              iterated = iterated + 1
            end
            assert(iterated == root:child_count())

            local edited = tree:edit(6, 6, 7, 0, 6, 0, 6, 0, 7)
            assert(edited ~= tree)
            assert(edited:root():byte_length() == tree:root():byte_length() + 1)
            assert(tree:copy():root():equal(tree:root()) == false)

            local query = vim._ts_parse_query(parser_language, '(_) @node')
            local query_info = query:inspect()
            assert(query_info.captures[1] == 'node')
            local cursor = vim._create_ts_querycursor(root, query, {
              start_row = 0, start_col = 0, end_row = 100, end_col = 0,
              match_limit = 1024,
            })
            local capture_id, captured, matched = cursor:next_capture()
            assert(capture_id == 1)
            assert(captured:tree() == tree)
            local match_id, pattern = matched:info()
            assert(type(match_id) == 'number' and pattern == 1)
            assert(type(matched:captures()[1]) == 'table')
            cursor:remove_match(match_id)

            local match_cursor = vim._create_ts_querycursor(root, query, {
              start_row = 0, start_col = 0, end_row = 100, end_col = 0,
            })
            local next_match = match_cursor:next_match()
            assert(next_match ~= nil)
            local next_id = next_match:info()
            match_cursor:remove_match(next_id)

            assert(not pcall(vim._ts_parse_query, parser_language, '('))
            assert(not pcall(vim._create_ts_parser, '__missing_language__'))
            assert(vim._ts_remove_language(parser_language))
            assert(not vim._ts_has_language(parser_language))
            collectgarbage('collect')
            assert(root:byte_length() == eb - sb)
            assert(query:inspect().captures[1] == 'node')

            return { logs = function() return logs end }
            ",
        )
        .eval()
        .unwrap();

    scheduler.drain().unwrap();
    let logs: mlua::Function = result.get("logs").unwrap();
    assert!(
        logs.call::<u32>(()).unwrap() > 0,
        "real parser should emit scheduled logger records"
    );
}

#[test]
fn failing_treesitter_calls_reach_pcall_as_strings() {
    let (parser, language) = require_parser();

    let scheduler = Rc::new(TestScheduler::default());
    let host = LuaHost::new(runtime_root(), Rc::new(NoBuiltins), scheduler.clone()).unwrap();
    let lua = host.lua();
    lua.globals()
        .set("parser_path", parser.to_string_lossy().as_ref())
        .unwrap();
    lua.globals().set("parser_language", language).unwrap();

    lua.load(
        r"
        assert(vim._ts_add_language_from_object(parser_path, parser_language))
        local parser = vim._create_ts_parser(parser_language)

        -- Factory failures: bad query compile and unknown language.
        local ok, err = pcall(vim._ts_parse_query, parser_language, '(')
        assert(ok == false, 'bad query parse must fail')
        assert(type(err) == 'string', 'bad query parse error must be a string, got ' .. type(err))
        assert(#err > 0)

        local ok, err = pcall(vim._create_ts_parser, '__missing_language__')
        assert(ok == false and type(err) == 'string' and #err > 0)

        -- Userdata method failures: invalid node operations.
        local tree, ranges = parser:parse(nil, 'local value = 1')
        assert(type(tree) == 'userdata', 'parser:parse first return is ' .. type(tree))
        local root = tree:root()
        assert(type(root) == 'userdata', 'tree:root() returned ' .. type(root))

        local ok, err = pcall(root.child, root, -1)
        assert(ok == false and type(err) == 'string' and #err > 0)

        local ok, err = pcall(function() return root:child('not-a-number') end)
        assert(ok == false and type(err) == 'string' and #err > 0)

        local ok, err = pcall(function() return root:descendant_for_range(0, 0, -5, -5) end)
        assert(ok == false and type(err) == 'string' and #err > 0)

        local ok, err = pcall(parser.parse, parser, 123)
        assert(ok == false and type(err) == 'string' and #err > 0)

        local query = vim._ts_parse_query(parser_language, '(_) @node')
        local ok, err = pcall(query.disable_pattern, query, 99)
        assert(ok == false and type(err) == 'string' and #err > 0)

        -- Success paths are unchanged by the wrappers.
        assert(type(parser:parse(nil, 'local other = 2')) == 'userdata')
        assert(root:child_count() > 0)
        local child = root:child(0)
        assert(child ~= nil and child:parent():equal(root))
        ",
    )
    .eval::<()>()
    .unwrap();
}

#[test]
fn incremental_parse_reports_exact_changed_ranges() {
    let (parser, language) = require_parser();
    let scheduler = Rc::new(TestScheduler::default());
    let host = LuaHost::new(runtime_root(), Rc::new(NoBuiltins), scheduler.clone()).unwrap();
    let lua = host.lua();
    lua.globals()
        .set("parser_path", parser.to_string_lossy().as_ref())
        .unwrap();
    lua.globals().set("parser_language", language).unwrap();

    lua.load(
        r#"
        assert(vim._ts_add_language_from_object(parser_path, parser_language))
        PARSER = vim._create_ts_parser(parser_language)
        SOURCE = 'local value = 1\n'
        FIRST, INITIAL = PARSER:parse(nil, SOURCE, true)
        "#,
    )
    .eval::<()>()
    .unwrap();
    let initial: i64 = lua.load(r#"return #INITIAL"#).eval().unwrap();
    assert!(initial > 0, "initial parse has ranges");
    lua.load(
        r#"
        SAME_TREE, SAME = PARSER:parse(FIRST, SOURCE, true)
        "#,
    )
    .eval::<()>()
    .unwrap();
    let same: i64 = lua.load(r#"return #SAME"#).eval().unwrap();
    assert_eq!(same, 0, "identical reparse reports no changes");
    lua.load(
        r#"
        -- The old tree records the edit first (`tree:edit`), then the
        -- reparse reports exactly the changed span in new coordinates.
        EDITED_SOURCE = 'local value = true\n'
        EDITED_OLD = FIRST:edit(14, 15, 18, 0, 14, 0, 15, 0, 18)
        SECOND, CHANGED = PARSER:parse(EDITED_OLD, EDITED_SOURCE, true)
        "#,
    )
    .eval::<()>()
    .unwrap();
    let changed: i64 = lua.load(r#"return #CHANGED"#).eval().unwrap();
    assert_eq!(changed, 1, "one changed span");
    let span: mlua::Table = lua.load(r#"return CHANGED[1]"#).eval().unwrap();
    let get = |index: i64| -> i64 { span.raw_get(index).unwrap() };
    assert_eq!((get(1), get(2), get(3)), (0, 14, 14));
    assert_eq!((get(4), get(5), get(6)), (0, 18, 18));
    lua.load(
        r#"
        THIRD, QUADS = PARSER:parse(EDITED_OLD, EDITED_SOURCE)
        "#,
    )
    .eval::<()>()
    .unwrap();
    let quad_len: i64 = lua.load(r#"return #QUADS[1]"#).eval().unwrap();
    assert_eq!(quad_len, 4, "byte-free entries are row/col quads");
    let quads: i64 = lua.load(r#"return #QUADS"#).eval().unwrap();
    assert!(quads > 0, "quad reparse reports changes");
    lua.load(
        r#"
        RANGE_REJECTED = not pcall(function()
          PARSER:set_included_ranges({ { 'x' } })
        end)
        "#,
    )
    .eval::<()>()
    .unwrap();
    let rejected: bool = lua.load(r#"return RANGE_REJECTED"#).eval().unwrap();
    assert!(rejected, "malformed included ranges fail");
    scheduler.drain().unwrap();
}

#[test]
fn emit_highlights_filters_groups_coords_and_priority() {
    let (parser, language) = require_parser();
    let scheduler = Rc::new(TestScheduler::default());
    let host = LuaHost::new(runtime_root(), Rc::new(NoBuiltins), scheduler.clone()).unwrap();
    let lua = host.lua();
    lua.globals()
        .set("parser_path", parser.to_string_lossy().as_ref())
        .unwrap();
    lua.globals().set("parser_language", language).unwrap();

    lua.load(
        r#"
        assert(vim._ts_add_language_from_object(parser_path, parser_language))
        local parser = vim._create_ts_parser(parser_language)
        local tree = parser:parse(nil, 'local value = 1\n')
        local root = tree:root()
        local query = vim._ts_parse_query(parser_language,
          '((identifier) @variable) ((number) @_hidden) ((chunk) @scope) ((identifier) @important (#set! "priority" "250"))')

        local recorded = {}
        vim.api.nvim_buf_set_extmark = function(bufnr, ns, row, col, opts)
          recorded[#recorded + 1] =
            { bufnr = bufnr, ns = ns, row = row, col = col, opts = opts }
          return 1
        end
        vim._ts_emit_highlights(root, query, 7, 9)
        assert(#recorded == 3)

        local seen = {}
        for _, call in ipairs(recorded) do
          assert(call.bufnr == 7 and call.ns == 9)
          local group = call.opts.hl_group
          assert(group ~= '@_hidden')
          assert(call.opts.strict == false)
          seen[group] = call
        end
        assert(seen['@variable'] ~= nil)
        assert(seen['@scope'] ~= nil)
        assert(seen['@important'] ~= nil)

        -- Identifier coordinates track the source text.
        local variable = seen['@variable']
        assert(variable.row == 0 and variable.col == 6)
        assert(variable.opts.end_row == 0 and variable.opts.end_col == 11)
        assert(variable.opts.priority == 100)

        -- Explicit priority survives; the chunk spans both lines.
        assert(seen['@important'].opts.priority == 250)
        local scope = seen['@scope']
        assert(scope.row == 0 and scope.opts.end_row == 1)
        "#,
    )
    .eval::<()>()
    .unwrap();
    scheduler.drain().unwrap();
}
