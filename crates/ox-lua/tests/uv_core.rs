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
        // The runtime prelude probes has('win32') during host init
        // (runtime/lua/vim/_core/system.lua).
        if name.as_bytes() == b"has" {
            return Ok(Typval::Number(0));
        }
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

#[test]
fn failing_fs_open_callback_receives_error_as_first_argument() {
    let (host, scheduler) = host();
    host.lua().globals().set("missing_path", "/definitely/not/a/real/ox-lua-file").unwrap();
    host.lua()
        .load(
            r#"
            open_error = nil
            vim.uv.fs_open(missing_path, 'r', 0, function(err, fd)
              assert(err ~= nil, 'error must arrive as the first callback argument')
              assert(fd == nil)
              open_error = err
            end)
            "#,
        )
        .exec()
        .unwrap();
    scheduler.drain().unwrap();
    let error = host.lua().globals().get::<String>("open_error").unwrap();
    assert!(error.contains("ENOENT"), "unexpected error string: {error}");
}

#[test]
fn pipe_write_callback_fires_only_when_the_loop_pumps_the_write() {
    let (host, scheduler) = host();
    host.lua()
        .load(
            r#"
            write_fired = false
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
              if not chunk then output:close() end
            end)
            -- Larger than the kernel pipe buffer, so queueing the write cannot
            -- also complete it.
            local payload = ('x'):rep(256 * 1024)
            input:write(payload, function(err)
              assert(err == nil)
              write_fired = true
              input:close()
            end)
            assert(write_fired == false, 'write callback fired before completion')
            vim.uv.run('default')
            assert(write_fired == true, 'write callback did not fire after the loop pumped the write')
            "#,
        )
        .exec()
        .unwrap();
    scheduler.drain().unwrap();
    assert!(host.lua().globals().get::<bool>("write_fired").unwrap());
}

/// Capacity of a fresh child-stdin pipe: one blocking oversized write fills
/// the pipe, and the `head -c 1` child exits after its single-byte read and
/// closes the read end, so the write returns the partial count it copied —
/// the pipe capacity — instead of blocking for a reader that never drains.
fn stdin_pipe_capacity() -> usize {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("head")
        .args(["-c", "1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pipe capacity probe");
    let mut stdin = child.stdin.take().expect("probe stdin pipe");
    let accepted = stdin.write(&vec![b'x'; 1 << 20]).expect("probe write");
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    accepted.max(1)
}

#[test]
fn pipe_write_flushing_earlier_write_parks_completion_until_borrow_release() {
    let (host, scheduler) = host();
    // Write A exceeds the pipe capacity by half of it, so it buffers a
    // remainder that a single later flush can carry together with write B.
    let capacity = stdin_pipe_capacity();
    let payload_a = capacity + capacity / 2;
    host.lua()
        .load(
            format!(
                r"
            order = ''
            local input = vim.uv.new_pipe(false)
            local process
            process = assert(vim.uv.spawn('/bin/cat', {{
              stdio = {{ input, nil, nil }},
            }}, function(code, signal)
              assert(code == 0 and signal == 0)
              process:close()
            end))
            -- Larger than the pipe capacity: write A returns with a buffered
            -- remainder, so its completion is still queued when write B runs.
            input:write(('a'):rep({payload_a}), function(err)
              assert(err == nil)
              order = order .. 'A'
              -- Close from inside the completion: this fires while write B's
              -- synchronous flush still holds the pipe borrow, and closing
              -- used to re-enter that borrow and panic.
              input:close()
            end)
            assert(order == '', 'write A completed before write B was queued')
            -- Let cat drain the pipe so write B's synchronous flush carries
            -- A's remainder and B in a single pass.
            local deadline = os.clock() + 0.25
            while os.clock() < deadline do end
            input:write('b', function(err)
              assert(err == nil)
              order = order .. 'B'
            end)
            vim.uv.run('default')
            assert(order == 'AB', 'completion order was: ' .. order)
            "
            )
            .as_str(),
        )
        .exec()
        .unwrap();
    scheduler.drain().unwrap();
    assert_eq!(host.lua().globals().get::<String>("order").unwrap(), "AB");
}

#[test]
fn timer_close_is_idempotent() {
    let (host, scheduler) = host();
    host.lua()
        .load(
            r#"
            local timer = vim.uv.new_timer()
            timer:start(60000, 0, function() end)
            timer:close()
            timer:close()
            assert(timer:is_closing())
            vim.uv.run('nowait')
            timer:close()
            closed_twice = true
            "#,
        )
        .exec()
        .unwrap();
    scheduler.drain().unwrap();
    assert!(host.lua().globals().get::<bool>("closed_twice").unwrap());
}
