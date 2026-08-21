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
        Err(format!("unexpected Vimscript builtin call: {}", name.to_string_lossy()))
    }
}

fn parser_from_environment() -> Option<(PathBuf, String)> {
    if let Some(path) = std::env::var_os("OXVIM_TREE_SITTER_PARSER").map(PathBuf::from) {
        let language = std::env::var("OXVIM_TREE_SITTER_LANGUAGE")
            .ok()
            .or_else(|| path.file_stem().and_then(|stem| stem.to_str()).map(str::to_owned))?;
        return path.is_file().then_some((path, language));
    }

    let root = std::env::var_os("OXVIM_REF_ROOT").map(PathBuf::from)?;
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

#[test]
fn real_parser_exercises_parse_nodes_edit_queries_and_lifetimes() {
    let Some((parser, language)) = parser_from_environment() else {
        println!("SKIP treesitter real-parser test: set OXVIM_TREE_SITTER_PARSER and OXVIM_TREE_SITTER_LANGUAGE, or provide OXVIM_REF_ROOT with a built Neovim parser");
        return;
    };

    let scheduler = Rc::new(TestScheduler::default());
    let host = LuaHost::new(runtime_root(), Rc::new(NoBuiltins), scheduler.clone()).unwrap();
    let lua = host.lua();
    lua.globals().set("parser_path", parser.to_string_lossy().as_ref()).unwrap();
    lua.globals().set("parser_language", language).unwrap();

    let result: mlua::Table = lua
        .load(
            r#"
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
            "#,
        )
        .eval()
        .unwrap();

    scheduler.drain().unwrap();
    let logs: mlua::Function = result.get("logs").unwrap();
    assert!(logs.call::<u32>(()).unwrap() > 0, "real parser should emit scheduled logger records");
}
