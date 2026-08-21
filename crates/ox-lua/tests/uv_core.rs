//! Lua-driven behavioral coverage for the essential `vim.uv` surface.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;

use ox_lua::{BuiltinHost, LuaHost, RuntimeRoot, Scheduler, Work};
use ox_types::{OxStr, Typval};

#[derive(Default)]
struct TestScheduler {
    queue: RefCell<VecDeque<Work>>,
}

impl TestScheduler {
    fn drain(&self) -> mlua::Result<()> {
        loop {
            let work = self.queue.borrow_mut().pop_front();
            let Some(work) = work else { break };
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

struct TestBuiltins;

impl BuiltinHost for TestBuiltins {
    fn call(&self, name: &OxStr, _args: Vec<Typval>) -> Result<Typval, String> {
        Err(format!("unexpected Vimscript call: {name:?}"))
    }
}

fn host() -> (LuaHost, Rc<TestScheduler>) {
    let scheduler = Rc::new(TestScheduler::default());
    let runtime = RuntimeRoot::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../runtime"));
    let host = LuaHost::new(runtime, Rc::new(TestBuiltins), scheduler.clone()).unwrap();
    (host, scheduler)
}

fn temp_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("oxvim-uv-{label}-{}", std::process::id()))
}

#[test]
fn timer_callback_runs_through_the_scheduler() {
    let (host, scheduler) = host();
    host.lua()
        .load(
            r#"
            timer_fired = false
            local timer = vim.uv.new_timer()
            assert(timer:start(0, 0, function()
              timer_fired = true
              timer:close()
            end))
            vim.uv.run('default')
            assert(timer_fired == false)
            "#,
        )
        .exec()
        .unwrap();
    scheduler.drain().unwrap();
    assert!(host.lua().globals().get::<bool>("timer_fired").unwrap());
}

#[test]
fn callback_file_read_preserves_bytes_and_error_first_shape() {
    let path = temp_path("file-read");
    std::fs::write(&path, b"a\0b\n").unwrap();
    let (host, scheduler) = host();
    host.lua().globals().set("test_path", path.to_string_lossy().as_ref()).unwrap();
    host.lua()
        .load(
            r#"
            file_result = false
            vim.uv.fs_open(test_path, 'r', 0, function(open_error, fd)
              assert(open_error == nil)
              vim.uv.fs_read(fd, 4, 0, function(read_error, bytes)
                assert(read_error == nil)
                assert(bytes == 'a\0b\n')
                assert(vim.uv.fs_close(fd))
                file_result = true
              end)
            end)
            "#,
        )
        .exec()
        .unwrap();
    scheduler.drain().unwrap();
    assert!(host.lua().globals().get::<bool>("file_result").unwrap());
    std::fs::remove_file(path).unwrap();
}

#[test]
fn spawned_cat_echoes_through_created_stdio_pipe() {
    let (host, scheduler) = host();
    host.lua()
        .load(
            r#"
            cat_result = ''
            local input = vim.uv.new_pipe(false)
            local output = vim.uv.new_pipe(false)
            local process
            process = assert(vim.uv.spawn('/bin/cat', {
              stdio = { input, output, nil },
            }, function(code, signal)
              assert(code == 0 and signal == 0)
              process:close()
            end))
            output:read_start(function(err, chunk)
              assert(err == nil)
              if chunk then cat_result = cat_result .. chunk else output:close() end
            end)
            input:write('cat echo\n', function(err)
              assert(err == nil)
              input:shutdown(function(shutdown_err)
                assert(shutdown_err == nil)
                input:close()
              end)
            end)
            vim.uv.run('default')
            "#,
        )
        .exec()
        .unwrap();
    scheduler.drain().unwrap();
    assert_eq!(host.lua().globals().get::<String>("cat_result").unwrap(), "cat echo\n");
}

#[test]
fn tcp_loopback_accepts_reads_and_writes() {
    let (host, scheduler) = host();
    host.lua()
        .load(
            r#"
            tcp_result = ''
            local server = vim.uv.new_tcp()
            assert(server:bind('127.0.0.1', 0))
            local address = assert(server:getsockname())
            assert(server:listen(16, function(err)
              assert(err == nil)
              local peer = vim.uv.new_tcp()
              assert(server:accept(peer))
              peer:read_start(function(read_err, chunk)
                assert(read_err == nil)
                if chunk then
                  peer:write(chunk)
                else
                  peer:close()
                  server:close()
                end
              end)
            end))
            local client = vim.uv.new_tcp()
            client:connect('127.0.0.1', address.port, function(err)
              assert(err == nil)
              client:read_start(function(read_err, chunk)
                assert(read_err == nil)
                if chunk then
                  tcp_result = tcp_result .. chunk
                  client:shutdown(function() client:close() end)
                end
              end)
              client:write('loopback')
            end)
            vim.uv.run('default')
            "#,
        )
        .exec()
        .unwrap();
    scheduler.drain().unwrap();
    assert_eq!(host.lua().globals().get::<String>("tcp_result").unwrap(), "loopback");
}

#[test]
fn new_thread_uses_an_isolated_lua_global_table() {
    let marker_path = temp_path("thread-isolation");
    let (host, _) = host();
    host.lua().globals().set("thread_marker", "parent-only").unwrap();
    host.lua().globals().set("marker_path", marker_path.to_string_lossy().as_ref()).unwrap();
    host.lua()
        .load(
            r#"
            local thread = assert(vim.uv.new_thread(function(path)
              assert(thread_marker == nil)
              thread_marker = 'child-only'
              local file = assert(io.open(path, 'wb'))
              assert(file:write(thread_marker))
              assert(file:close())
            end, marker_path))
            assert(thread:join())
            assert(thread_marker == 'parent-only')
            "#,
        )
        .exec()
        .unwrap();
    assert_eq!(std::fs::read_to_string(&marker_path).unwrap(), "child-only");
    std::fs::remove_file(marker_path).unwrap();
}
