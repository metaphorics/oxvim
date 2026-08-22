//! Stress regression for the LuaRef / auxiliary-stack lifecycle.
//!
//! mlua pins every Rust-held Lua value in one auxiliary thread whose stack
//! caps near 8000 slots; retaining one handle per event exhausts it with
//! `cannot create a Lua reference, out of auxiliary stack space`. These
//! tests drive more than 10k callback/reference cycles through the paths
//! that previously retained handles or registry entries forever and assert
//! the host stays memory-bounded (and simply completes, which the previous
//! leak made impossible: `uv.new_work` aborted at ~6k cycles).
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;

use mlua::{Function, Lua, Value};
use ox_lua::{
    free_lua_ref, lua_to_object, object_to_lua, BuiltinHost, LuaHost, RuntimeRoot, Scheduler, Work,
};
use ox_types::{Object, OxStr, Typval};

struct FakeScheduler {
    queue: RefCell<VecDeque<Work>>,
}

impl FakeScheduler {
    fn drain(&self) {
        loop {
            let work = self.queue.borrow_mut().pop_front();
            let Some(work) = work else { break };
            let _ = work();
        }
    }
}

impl Scheduler for FakeScheduler {
    fn schedule_deferred(&self, work: Work) -> Result<(), String> {
        self.queue.borrow_mut().push_back(work);
        Ok(())
    }
}

struct FakeBuiltins;
impl BuiltinHost for FakeBuiltins {
    fn call(&self, _name: &OxStr, _args: Vec<Typval>) -> Result<Typval, String> {
        Ok(Typval::Number(1))
    }
}

fn host() -> (LuaHost, Rc<FakeScheduler>) {
    let scheduler = Rc::new(FakeScheduler { queue: RefCell::new(VecDeque::new()) });
    let root = RuntimeRoot::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../runtime"));
    let host = LuaHost::new(root, Rc::new(FakeBuiltins), scheduler.clone()).expect("host");
    (host, scheduler)
}

fn settled_memory(lua: &Lua) -> usize {
    lua.gc_collect().unwrap_or_else(|_| lua.gc_restart());
    lua.used_memory()
}

/// A leak of more than a few hundred KiB across 12k cycles; the previous
/// defects grew 2-4 MiB or aborted outright.
const GROWTH_BUDGET_BYTES: usize = 512 * 1024;

const CYCLES: usize = 12_000;

#[test]
fn uv_work_handles_do_not_exhaust_auxiliary_stack() {
    let (mut host, _) = host();
    let lua = host.lua().clone();
    let mut baseline = None;
    for cycle in 0..CYCLES {
        // Each call previously leaked one retained after-work Function.
        host.exec(
            "local w = vim.uv.new_work(function() return 1 end, function() end) \
             w:queue() collectgarbage()",
            vec![],
        )
        .expect("new_work cycle");
        if cycle == 1_000 {
            baseline = Some(settled_memory(&lua));
        }
    }
    let growth = settled_memory(&lua).saturating_sub(baseline.expect("baseline"));
    assert!(
        growth <= GROWTH_BUDGET_BYTES,
        "uv.new_work cycles leaked {growth} bytes"
    );
}

#[test]
fn vim_fn_funcref_arguments_release_their_lua_refs() {
    let (mut host, _) = host();
    let lua = host.lua().clone();
    let mut baseline = None;
    for cycle in 0..CYCLES {
        // The function argument is stored as a LuaRef for the builtin call
        // and must be released once the call returns (nlua_call parity).
        host.exec("return vim.call('abs', { f = function() end })", vec![])
            .expect("vim.call cycle");
        if cycle == 1_000 {
            baseline = Some(settled_memory(&lua));
        }
    }
    let growth = settled_memory(&lua).saturating_sub(baseline.expect("baseline"));
    assert!(
        growth <= GROWTH_BUDGET_BYTES,
        "vim.call funcref arguments leaked {growth} bytes"
    );
}

#[test]
fn exec_results_stay_bounded_when_caller_releases_refs() {
    let (mut host, _) = host();
    let lua = host.lua().clone();
    let mut baseline = None;
    for cycle in 0..CYCLES {
        let result = host
            .exec("local t = {1, 2, 3}; return function() return t end", vec![])
            .expect("exec cycle");
        // Server-shaped ownership: the reply encoder consumes the result and
        // releases every LuaRef it contains (ox-rpc encodes `<Lua N>` text).
        for reference in object_refs(&result) {
            free_lua_ref(&lua, reference).expect("free_lua_ref");
        }
        if cycle == 1_000 {
            baseline = Some(settled_memory(&lua));
        }
    }
    let growth = settled_memory(&lua).saturating_sub(baseline.expect("baseline"));
    assert!(
        growth <= GROWTH_BUDGET_BYTES,
        "released exec LuaRefs leaked {growth} bytes"
    );
}

#[test]
fn uv_timer_and_pipe_callback_cycles_stay_bounded() {
    let (mut host, scheduler) = host();
    let lua = host.lua().clone();
    let mut baseline = None;
    for cycle in 0..CYCLES {
        host.exec(
            "local t = vim.uv.new_timer() \
             t:start(1, 0, function() end) \
             vim._core.loop_poll(-1) \
             t:close() \
             vim._core.loop_poll(-1) \
             local p = vim.uv.new_pipe() \
             p:write('x', function() end) \
             p:close()",
            vec![],
        )
        .expect("timer/pipe cycle");
        scheduler.drain();
        if cycle == 1_000 {
            baseline = Some(settled_memory(&lua));
        }
    }
    let growth = settled_memory(&lua).saturating_sub(baseline.expect("baseline"));
    assert!(
        growth <= GROWTH_BUDGET_BYTES,
        "timer/pipe callback cycles leaked {growth} bytes"
    );
}

#[test]
fn freed_lua_refs_recycle_slots_and_fail_to_reload() {
    let (host, _) = host();
    let lua = host.lua();
    let first: Function = lua.load("return function(x) return x + 1 end").eval().unwrap();
    let second: Function = lua.load("return function(x) return x + 2 end").eval().unwrap();
    let a = match lua_to_object(lua, &Value::Function(first)).unwrap() {
        Object::LuaRef(reference) => reference,
        other => panic!("expected LuaRef, got {other:?}"),
    };
    let b = match lua_to_object(lua, &Value::Function(second)).unwrap() {
        Object::LuaRef(reference) => reference,
        other => panic!("expected LuaRef, got {other:?}"),
    };
    assert_ne!(a, b);

    // Live refs round-trip.
    let loaded: Function = match object_to_lua(lua, &Object::LuaRef(a)).unwrap() {
        Value::Function(function) => function,
        other => panic!("expected function, got {other:?}"),
    };
    assert_eq!(loaded.call::<i64>(1).unwrap(), 2);

    // Freeing is idempotent and makes the slot unavailable.
    free_lua_ref(lua, a).unwrap();
    free_lua_ref(lua, a).unwrap();
    assert!(object_to_lua(lua, &Object::LuaRef(a)).is_err());

    // The freed slot is recycled by the next store instead of growing.
    let third: Function = lua.load("return function(x) return x + 3 end").eval().unwrap();
    let c = match lua_to_object(lua, &Value::Function(third)).unwrap() {
        Object::LuaRef(reference) => reference,
        other => panic!("expected LuaRef, got {other:?}"),
    };
    assert_eq!(a, c);
    let reloaded: Function = match object_to_lua(lua, &Object::LuaRef(c)).unwrap() {
        Value::Function(function) => function,
        other => panic!("expected function, got {other:?}"),
    };
    assert_eq!(reloaded.call::<i64>(1).unwrap(), 4);

    // The untouched ref is unaffected.
    let kept: Function = match object_to_lua(lua, &Object::LuaRef(b)).unwrap() {
        Value::Function(function) => function,
        other => panic!("expected function, got {other:?}"),
    };
    assert_eq!(kept.call::<i64>(1).unwrap(), 3);
}

/// Collect every LuaRef id in an object graph.
fn object_refs(object: &Object) -> Vec<i32> {
    let mut refs = Vec::new();
    collect_refs(object, &mut refs);
    refs
}

fn collect_refs(object: &Object, refs: &mut Vec<i32>) {
    match object {
        Object::LuaRef(reference) => refs.push(*reference),
        Object::Array(items) => items.iter().for_each(|item| collect_refs(item, refs)),
        Object::Dict(pairs) => pairs.iter().for_each(|(_, item)| collect_refs(item, refs)),
        _ => {}
    }
}
