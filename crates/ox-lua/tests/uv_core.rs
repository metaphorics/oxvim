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

struct ImmediateScheduler;

impl Scheduler for ImmediateScheduler {
    fn schedule_deferred(&self, work: Work) -> Result<(), String> {
        work().map_err(|error| error.to_string())
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

/// Restores one process environment binding on drop: a value present before
/// the test is written back, absence is restored by unsetting. Goes through
/// the same `ox-uv` seam the bindings use, so restoration needs no `unsafe`.
struct EnvGuard {
    name: &'static str,
    prior: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn take(name: &'static str) -> Self {
        Self {
            name,
            prior: std::env::var_os(name),
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        let _ = match &self.prior {
            Some(value) => ox_uv::misc::os_setenv(self.name, value),
            None => ox_uv::misc::os_unsetenv(self.name),
        };
    }
}

#[test]
fn sleep_blocks_and_returns_no_values() {
    let (host, _scheduler) = host();
    let started = std::time::Instant::now();
    let (returns, alias_returns) = host
        .lua()
        .load("return select('#', vim.uv.sleep(20)), select('#', vim.loop.sleep(-1))")
        .eval::<(i64, i64)>()
        .unwrap();

    assert_eq!(returns, 0);
    assert_eq!(alias_returns, 0);
    assert!(started.elapsed() >= std::time::Duration::from_millis(20));
}

#[test]
fn timer_callback_runs_synchronously_as_fast_event() {
    let (host, _scheduler) = host();
    host.lua()
        .load(
            r"
            timer_fired = false
            local timer = vim.uv.new_timer()
            assert(timer:start(0, 0, function()
              assert(vim.in_fast_event())
              timer_fired = true
              vim.uv.stop()
              timer:stop()
              timer:close()
            end))
            vim.uv.run('default')
            assert(timer_fired)
            assert(timer:is_closing())
            ",
        )
        .exec()
        .unwrap();
}

#[test]
fn immediate_timer_callback_can_close_its_handle() {
    let runtime = RuntimeRoot::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../runtime"));
    let host = LuaHost::new(runtime, Rc::new(TestBuiltins), Rc::new(ImmediateScheduler)).unwrap();
    host.lua()
        .load(
            r"
            local timer = vim.uv.new_timer()
            timer:start(0, 0, function()
              assert(not timer:is_closing())
              vim.uv.stop()
              timer:stop()
              timer:close()
            end)
            vim.uv.run('default')
            assert(timer:is_closing())
            ",
        )
        .exec()
        .unwrap();
}

#[cfg(unix)]
#[test]
fn immediate_pipe_callback_can_close_process_handles() {
    let runtime = RuntimeRoot::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../runtime"));
    let host = LuaHost::new(runtime, Rc::new(TestBuiltins), Rc::new(ImmediateScheduler)).unwrap();
    host.lua()
        .load(
            r"
            local output = vim.uv.new_pipe(false)
            local process
            process = assert(vim.uv.spawn('/bin/true', { stdio = { nil, output, nil } }, function()
              if not process:is_closing() then process:close() end
            end))
            output:read_start(function(err, chunk)
              assert(err == nil)
              if chunk == nil and not output:is_closing() then output:close() end
            end)
            vim.uv.run('default')
            ",
        )
        .exec()
        .unwrap();
}

#[cfg(unix)]
#[test]
fn signal_binding_supports_luv_module_and_method_forms() {
    let (host, _) = host();
    host.lua()
        .load(
            r"
            local signal = assert(vim.uv.new_signal())
            assert(vim.uv.signal_start(signal, 'sigpipe', function(signame)
              assert(signame == 'sigpipe')
            end) == 0)
            assert(vim.uv.signal_stop(signal) == 0)
            assert(signal:start_oneshot('sigpipe', function(signame)
              assert(signame == 'sigpipe')
            end) == 0)
            assert(signal:stop() == 0)
            assert(not signal:is_closing())
            signal:close()
            assert(signal:is_closing())
            vim.uv.run('nowait')
            ",
        )
        .exec()
        .unwrap();
}

#[cfg(unix)]
#[test]
fn signal_status_reads_live_loop_inside_callback() {
    let (host, _) = host();
    host.lua()
        .load(
            r"
            local signal = assert(vim.uv.new_signal())
            signal_callback_ran = false
            signal_closing = true
            assert(signal:start_oneshot('sigusr1', function(signame)
              assert(signame == 'sigusr1')
              signal_callback_ran = true
              signal_closing = signal:is_closing()
              signal:close()
            end) == 0)
            ",
        )
        .exec()
        .unwrap();
    let raiser = std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(5));
        signal_hook::low_level::raise(signal_hook::consts::SIGUSR1)
            .expect("raise SIGUSR1 for the callback");
    });
    host.lua().load("vim.uv.run('default')").exec().unwrap();
    raiser.join().expect("signal raiser must finish");
    assert!(
        host.lua()
            .globals()
            .get::<bool>("signal_callback_ran")
            .unwrap()
    );
    assert!(
        !host.lua().globals().get::<bool>("signal_closing").unwrap(),
        "signal callback must see its open handle"
    );
}

#[cfg(unix)]
#[test]
fn fs_event_binding_lifecycle_reports_enoent_and_closes() {
    let (host, _) = host();
    let dir = std::env::temp_dir()
        .to_str()
        .expect("temp dir must be UTF-8")
        .to_owned();
    host.lua()
        .load(format!(
            r"
            local handle = assert(vim.uv.new_fs_event())
            local ok, err, name = handle:start('/definitely/not/here-oxvim', {{}}, function() end)
            assert(ok == nil and name == 'ENOENT', tostring(err))
            assert(handle:start('{dir}', {{}}, function() end) == 0)
            assert(handle:stop() == 0)
            assert(not handle:is_closing())
            handle:close()
            assert(handle:is_closing())
            vim.uv.run('nowait')
            "
        ))
        .exec()
        .unwrap();
}

#[test]
fn fs_event_backend_start_failure_preserves_idle_handle() {
    let dir = fresh_dir("backend-start-failure");
    let (host, scheduler) = host();
    host.lua()
        .globals()
        .set("test_dir", dir.to_string_lossy().as_ref())
        .unwrap();
    drive(
        &host,
        &scheduler,
        r#"
        local handle = assert(vim.uv.new_fs_event())
        local ok, err, name = handle:start(
          test_dir,
          {watch_entry = true, recursive = true},
          function() end
        )
        assert(ok == nil and name == 'ENOTSUP', tostring(err))
        assert(
          err == 'ENOTSUP: watch_entry cannot be combined with recursive',
          tostring(err)
        )
        local path, path_err, path_name = handle:getpath()
        assert(path == nil and path_name == 'EINVAL', tostring(path_err))
        assert(handle:start(test_dir, {}, function() end) == 0)
        assert(handle:getpath() == test_dir)
        handle:close()
        "#,
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn fs_event_start_inside_callback_snapshots_before_mutation() {
    let dir = fresh_dir("start-callback");
    let (host, scheduler) = host();
    host.lua()
        .globals()
        .set("test_dir", dir.to_string_lossy().as_ref())
        .unwrap();
    drive(
        &host,
        &scheduler,
        r#"
        local handle = assert(vim.uv.new_fs_event())
        local trigger = assert(vim.uv.new_timer())
        local guard = assert(vim.uv.new_timer())
        local timed_out = false
        local event_name

        guard:start(3000, 0, function()
          timed_out = true
          if not handle:is_closing() then handle:close() end
          guard:close()
          vim.uv.stop()
        end)
        trigger:start(0, 0, function()
          assert(handle:start(test_dir, {}, function(err, filename)
            assert(err == nil)
            event_name = filename
            if not handle:is_closing() then handle:close() end
            if not guard:is_closing() then guard:close() end
          end) == 0)
          assert(handle:getpath() == test_dir)

          local fd = assert(vim.uv.fs_open(test_dir .. '/created.txt', 'w', tonumber('644', 8)))
          assert(vim.uv.fs_write(fd, 'event') == 5)
          assert(vim.uv.fs_close(fd))
          trigger:close()
        end)
        vim.uv.run('default')
        assert(not timed_out, 'fs event did not observe the post-start mutation')
        assert(event_name == 'created.txt', tostring(event_name))
        "#,
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn fs_event_rejects_mistyped_options_and_coerces_numbers() {
    let dir = fresh_dir("options");
    let (host, scheduler) = host();
    host.lua()
        .globals()
        .set("test_dir", dir.to_string_lossy().as_ref())
        .unwrap();
    drive(
        &host,
        &scheduler,
        r#"
        for _, key in ipairs({'recursive', 'stat', 'watch_entry'}) do
          local handle = assert(vim.uv.new_fs_event())
          local ok, err = pcall(handle.start, handle, test_dir, {[key] = 'invalid'}, function() end)
          assert(not ok, key .. ' must reject a string option')
          -- mlua appends a traceback to the error object, so pin the exact
          -- message as a plain-text match rather than by whole-string equality.
          local want = "Invalid '" .. key .. "': not a boolean"
          assert(tostring(err):find(want, 1, true) ~= nil, tostring(err))

          assert(handle:start(test_dir, {[key] = 1}, function() end) == 0)
          assert(handle:stop() == 0)
          handle:close()
        end
        "#,
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(unix)]
#[test]
fn fs_event_callback_preserves_non_utf8_filename_bytes() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let dir = fresh_dir("non-utf8");
    let filename = OsString::from_vec(vec![b'n', b'o', b'n', 0xff, b'8']);
    let (host, scheduler) = host();
    host.lua()
        .globals()
        .set("test_dir", dir.to_string_lossy().as_ref())
        .unwrap();
    host.lua()
        .load(
            r#"
            local handle = assert(vim.uv.new_fs_event())
            local guard = assert(vim.uv.new_timer())
            timed_out = false
            event_name = nil
            guard:start(3000, 0, function()
              timed_out = true
              if not handle:is_closing() then handle:close() end
              guard:close()
              vim.uv.stop()
            end)
            assert(handle:start(test_dir, {}, function(err, filename)
              assert(err == nil)
              event_name = filename
              if not handle:is_closing() then handle:close() end
              if not guard:is_closing() then guard:close() end
            end) == 0)
            "#,
        )
        .exec()
        .unwrap();
    std::fs::write(dir.join(&filename), b"payload").unwrap();
    drive(
        &host,
        &scheduler,
        r#"
        vim.uv.run('default')
        assert(not timed_out, 'fs event did not report the non-UTF-8 filename')
        assert(event_name == string.char(110, 111, 110, 255, 56), 'filename bytes changed')
        "#,
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(unix)]
#[test]
fn fs_event_start_preserves_non_utf8_watch_path_bytes() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let dir = fresh_dir("non-utf8-watch-path");
    let watched_name = OsString::from_vec(vec![b'w', b'a', b't', 0xff, b'h']);
    let watched = dir.join(&watched_name);
    std::fs::create_dir(&watched).unwrap();
    let filename = OsString::from_vec(vec![b'e', b'v', 0xfe, b'8']);
    let watched_bytes = watched.as_os_str().as_encoded_bytes().to_vec();
    let (host, scheduler) = host();
    let lua_path = host.lua().create_string(&watched_bytes).unwrap();
    host.lua().globals().set("test_dir", lua_path).unwrap();
    host.lua()
        .load(
            r#"
            local handle = assert(vim.uv.new_fs_event())
            local guard = assert(vim.uv.new_timer())
            timed_out = false
            event_name = nil
            guard:start(3000, 0, function()
              timed_out = true
              if not handle:is_closing() then handle:close() end
              guard:close()
              vim.uv.stop()
            end)
            assert(handle:start(test_dir, {}, function(err, filename)
              assert(err == nil)
              event_name = filename
              if not handle:is_closing() then handle:close() end
              if not guard:is_closing() then guard:close() end
            end) == 0)
            assert(handle:getpath() == test_dir, 'watch path bytes changed')
            "#,
        )
        .exec()
        .unwrap();
    std::fs::write(watched.join(&filename), b"payload").unwrap();
    drive(
        &host,
        &scheduler,
        r#"
        vim.uv.run('default')
        assert(not timed_out, 'fs event did not report the non-UTF-8 watch path')
        assert(event_name == string.char(101, 118, 254, 56), 'filename bytes changed')
        "#,
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn wait_primitives_poll_the_owned_uv_loop() {
    let (host, scheduler) = host();
    host.lua()
        .load(
            r"
            poll_fired = false
            local timer = vim.uv.new_timer()
            timer:start(1, 0, function()
              poll_fired = true
            end)
            vim._core.ui_flush()
            assert(vim._core.check_interrupt() == false)
            vim._core.loop_poll(5, false)
            timer:close()
            ",
        )
        .exec()
        .unwrap();
    scheduler.drain().unwrap();
    assert!(host.lua().globals().get::<bool>("poll_fired").unwrap());
}

#[test]
fn phase_handles_support_between_case_cleanup() {
    let (host, _) = host();
    host.lua()
        .load(
            r"
            for _, constructor in ipairs({
              vim.uv.new_idle,
              vim.uv.new_prepare,
              vim.uv.new_check,
            }) do
              local handle = assert(constructor())
              assert(not handle:is_closing())
              handle:close()
              assert(handle:is_closing())
            end
            vim.wait(0)
            ",
        )
        .exec()
        .unwrap();
}

#[test]
fn phase_status_reads_live_loop_inside_callback() {
    let (host, _) = host();
    host.lua()
        .load(
            r"
            local phase = assert(vim.uv.new_idle())
            phase_active = false
            phase_closing = true
            assert(phase:start(function()
              phase_active = phase:is_active()
              phase_closing = phase:is_closing()
              phase:stop()
              phase:close()
            end))
            vim.uv.run('nowait')
            assert(phase_active, 'phase callback must see its active handle')
            assert(not phase_closing, 'phase callback must see its open handle')
            ",
        )
        .exec()
        .unwrap();
}

#[test]
fn callback_file_read_preserves_bytes_and_error_first_shape() {
    let path = temp_path("file-read");
    std::fs::write(&path, b"a\0b\n").unwrap();
    let (host, scheduler) = host();
    host.lua()
        .globals()
        .set("test_path", path.to_string_lossy().as_ref())
        .unwrap();
    host.lua()
        .load(
            r"
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
            ",
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
            r"
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
            ",
        )
        .exec()
        .unwrap();
    scheduler.drain().unwrap();
    assert_eq!(
        host.lua().globals().get::<String>("cat_result").unwrap(),
        "cat echo\n"
    );
}

#[cfg(unix)]
#[test]
fn process_pipe_close_delivers_stdin_eof_and_process_exit() {
    let runtime = RuntimeRoot::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../runtime"));
    let host = LuaHost::new(runtime, Rc::new(TestBuiltins), Rc::new(ImmediateScheduler)).unwrap();
    host.lua()
        .load(
            r"
            pipe_exit_code = nil
            pipe_exit_signal = nil
            local input = assert(vim.uv.new_pipe(false))
            local process
            process = assert(vim.uv.spawn('/bin/cat', {
              stdio = { input, nil, nil },
            }, function(code, signal)
              pipe_exit_code = code
              pipe_exit_signal = signal
              if not process:is_closing() then process:close() end
            end))
            pipe_pid = assert(process:get_pid())
            input:close()
            input:close()
            ",
        )
        .exec()
        .unwrap();

    let pid = host.lua().globals().get::<u32>("pipe_pid").unwrap();
    let (cancel_tx, cancel_rx) = std::sync::mpsc::channel();
    let watchdog = std::thread::spawn(move || {
        let timed_out = cancel_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .is_err();
        if timed_out {
            let _ = ox_uv::process::kill(pid, Some(15));
        }
        timed_out
    });

    let run_result = host.lua().load("vim.uv.run('default')").exec();
    let _ = cancel_tx.send(());
    let timed_out = watchdog.join().unwrap();
    assert!(!timed_out, "closing process stdin did not deliver EOF");
    run_result.unwrap();
    assert_eq!(
        host.lua().globals().get::<i32>("pipe_exit_code").unwrap(),
        0
    );
    assert_eq!(
        host.lua().globals().get::<i32>("pipe_exit_signal").unwrap(),
        0
    );
}

#[cfg(unix)]
#[test]
fn spawn_accepts_luv_stdio_environment_args_and_exit_callback() {
    let (host, scheduler) = host();
    host.lua()
        .load(
            r#"
            spawn_result = ''
            spawn_exited = false
            local output = assert(vim.uv.new_pipe(false))
            local process
            process = assert(vim.uv.spawn('/bin/sh', {
              stdio = { false, output, 2, nil },
              args = { '-c', 'printf \"%s:%s:%s:%s\" \"$ONLY\" \"$#\" \"$1\" \"${HOME-unset}\"', 'shell-name', 'marker' },
              env = { 'ONLY=exact' },
            }, function(code, signal)
              assert(code == 0 and signal == 0)
              spawn_exited = true
              process:close()
            end))
            output:read_start(function(err, chunk)
              assert(err == nil)
              if chunk then spawn_result = spawn_result .. chunk else output:close() end
            end)
            vim.uv.run('default')
            "#,
        )
        .exec()
        .unwrap();
    scheduler.drain().unwrap();
    assert_eq!(
        host.lua().globals().get::<String>("spawn_result").unwrap(),
        "exact:1:marker:unset",
    );
    assert!(host.lua().globals().get::<bool>("spawn_exited").unwrap());
}

#[cfg(unix)]
#[test]
fn abandoned_spawn_handles_do_not_keep_the_loop_alive() {
    let (host, scheduler) = host();
    host.lua()
        .load(
            r"
            do
              local input = assert(vim.uv.new_pipe(false))
              assert(vim.uv.spawn('/bin/cat', { stdio = { input, nil, nil } }, function() end))
            end
            collectgarbage('collect')
            vim.uv.run('default')
            ",
        )
        .exec()
        .unwrap();
    scheduler.drain().unwrap();
}

#[test]
fn tcp_loopback_accepts_reads_and_writes() {
    let (host, scheduler) = host();
    host.lua()
        .load(
            r"
            tcp_result = ''
            local server = vim.uv.new_tcp()
            assert(server:bind('127.0.0.1', 0))
            local address = assert(server:getsockname())
            assert(server:listen(16, function(err)
              assert(err == nil)
              assert(not server:is_closing(), 'listen callback must see its open handle')
              local peer = vim.uv.new_tcp()
              assert(server:accept(peer))
              peer:read_start(function(read_err, chunk)
                assert(read_err == nil)
                if chunk then
                  assert(not peer:is_closing(), 'read callback must see its open handle')
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
              assert(not client:is_closing(), 'connect callback must see its open handle')
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
            ",
        )
        .exec()
        .unwrap();
    scheduler.drain().unwrap();
    assert_eq!(
        host.lua().globals().get::<String>("tcp_result").unwrap(),
        "loopback"
    );
}

#[test]
fn new_thread_uses_an_isolated_lua_global_table() {
    let marker_path = temp_path("thread-isolation");
    let (host, _) = host();
    host.lua()
        .globals()
        .set("thread_marker", "parent-only")
        .unwrap();
    host.lua()
        .globals()
        .set("marker_path", marker_path.to_string_lossy().as_ref())
        .unwrap();
    host.lua()
        .load(
            r"
            local thread = assert(vim.uv.new_thread(function(path)
              assert(thread_marker == nil)
              thread_marker = 'child-only'
              local file = assert(io.open(path, 'wb'))
              assert(file:write(thread_marker))
              assert(file:close())
            end, marker_path))
            assert(thread:join())
            assert(thread_marker == 'parent-only')
            ",
        )
        .exec()
        .unwrap();
    assert_eq!(std::fs::read_to_string(&marker_path).unwrap(), "child-only");
    std::fs::remove_file(marker_path).unwrap();
}

#[test]
fn failing_fs_open_callback_receives_error_as_first_argument() {
    let (host, scheduler) = host();
    host.lua()
        .globals()
        .set("missing_path", "/definitely/not/a/real/ox-lua-file")
        .unwrap();
    host.lua()
        .load(
            r"
            open_error = nil
            vim.uv.fs_open(missing_path, 'r', 0, function(err, fd)
              assert(err ~= nil, 'error must arrive as the first callback argument')
              assert(fd == nil)
              open_error = err
            end)
            ",
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
            r"
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
            ",
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

#[cfg(unix)]
#[test]
fn nested_run_drains_pipe_close_requested_by_write_callback() {
    let (host, scheduler) = host();
    host.lua()
        .load(
            r"
            nested_completed = false
            process_exited = false
            local input = assert(vim.uv.new_pipe(false))
            local output = assert(vim.uv.new_pipe(false))
            local process
            process = assert(vim.uv.spawn('/bin/cat', {
              stdio = { input, output, nil },
            }, function(code, signal)
              assert(code == 0 and signal == 0)
              process_exited = true
              process:close()
            end))
            output:read_start(function(err, chunk)
              assert(err == nil)
              if chunk then
                input:close()
                while not process_exited do
                  vim.uv.run('once')
                end
                nested_completed = true
              else
                output:close()
              end
            end)
            input:write('x')
            vim.uv.run('default')
            assert(nested_completed, 'read callback did not complete its nested wait')
            ",
        )
        .exec()
        .unwrap();
    scheduler.drain().unwrap();
    assert!(
        host.lua()
            .globals()
            .get::<bool>("nested_completed")
            .unwrap()
    );
    assert!(host.lua().globals().get::<bool>("process_exited").unwrap());
}

#[test]
fn timer_close_is_idempotent() {
    let (host, scheduler) = host();
    host.lua()
        .load(
            r"
            local timer = vim.uv.new_timer()
            timer:start(60000, 0, function() end)
            timer:close()
            timer:close()
            assert(timer:is_closing())
            vim.uv.run('nowait')
            timer:close()
            closed_twice = true
            ",
        )
        .exec()
        .unwrap();
    scheduler.drain().unwrap();
    assert!(host.lua().globals().get::<bool>("closed_twice").unwrap());
}

fn fresh_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("oxvim-uvfs-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn drive(host: &LuaHost, scheduler: &Rc<TestScheduler>, script: &str) {
    host.lua().load(script).exec().unwrap();
    scheduler.drain().unwrap();
}

#[test]
fn fs_metadata_surface_round_trips_on_a_real_file() {
    let dir = fresh_dir("meta");
    let (host, scheduler) = host();
    host.lua()
        .globals()
        .set("test_dir", dir.to_string_lossy().as_ref())
        .unwrap();
    drive(
        &host,
        &scheduler,
        r"
        local uv = vim.uv
        local path = test_dir .. '/data.txt'
        local fd = assert(uv.fs_open(path, 'w', tonumber('644', 8)))
        assert(uv.fs_write(fd, 'hello uv', 0) == 8)
        local st = assert(uv.fs_fstat(fd))
        assert(st.size == 8 and st.type == 'file' and st.ino > 0 and st.mtime.sec > 0)
        assert(uv.fs_ftruncate(fd, 5))
        assert(uv.fs_fdatasync(fd))
        assert(uv.fs_fsync(fd))
        assert(uv.fs_close(fd))

        local st2 = assert(uv.fs_stat(path))
        assert(st2.size == 5 and st2.type == 'file' and st2.blksize > 0)
        assert(uv.fs_realpath(path) == path)
        assert(uv.fs_access(path, 'rw') == true)
        local denied, _, denied_name = uv.fs_access(path, 'x')
        assert(denied == nil and denied_name == 'EACCES', denied_name)

        assert(uv.fs_chmod(path, tonumber('600', 8)))
        local st3 = assert(uv.fs_stat(path))
        assert(st3.mode % 512 == tonumber('600', 8), st3.mode)

        assert(uv.fs_utime(path, 1000000000, 1000000000))
        assert(assert(uv.fs_stat(path)).mtime.sec == 1000000000)
        assert(uv.fs_utime(path, 'now', 'omit'))
        assert(assert(uv.fs_stat(path)).mtime.sec == 1000000000)

        local fd2 = assert(uv.fs_open(path, 'r+', 0))
        assert(uv.fs_fchmod(fd2, tonumber('640', 8)))
        assert(uv.fs_futime(fd2, 1234567890, 1234567890))
        assert(assert(uv.fs_fstat(fd2)).mtime.sec == 1234567890)
        assert(uv.fs_truncate(path, 2))
        assert(assert(uv.fs_stat(path)).size == 2)
        assert(uv.fs_close(fd2))
        ",
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn fs_directory_surface_round_trips_through_scandir_and_links() {
    let dir = fresh_dir("dirs");
    std::fs::write(dir.join("a.txt"), b"aa").unwrap();
    std::fs::write(dir.join("b.txt"), b"bbbb").unwrap();
    let (host, scheduler) = host();
    host.lua()
        .globals()
        .set("test_dir", dir.to_string_lossy().as_ref())
        .unwrap();
    drive(
        &host,
        &scheduler,
        r"
        local uv = vim.uv

        -- mkdir / rmdir
        assert(uv.fs_mkdir(test_dir .. '/sub', tonumber('755', 8)))
        assert(uv.fs_mkdir(test_dir .. '/sub/deep', tonumber('755', 8)))
        assert(uv.fs_rmdir(test_dir .. '/sub/deep'))
        assert(uv.fs_rmdir(test_dir .. '/sub'))

        -- scandir + scandir_next
        local scan = assert(uv.fs_scandir(test_dir))
        local names, types = {}, {}
        local name, ftype = uv.fs_scandir_next(scan)
        while name do
          names[#names + 1] = name
          types[name] = ftype
          name, ftype = uv.fs_scandir_next(scan)
        end
        table.sort(names)
        assert(#names == 2 and names[1] == 'a.txt' and names[2] == 'b.txt', table.concat(names, ','))
        assert(types['a.txt'] == 'file')

        -- mkdtemp / mkstemp
        local made = assert(uv.fs_mkdtemp(test_dir .. '/mkXXXXXX'))
        assert(vim.startswith(made, test_dir .. '/mk') and #made == #test_dir + 9)
        assert(uv.fs_rmdir(made))
        local fd, tmp = assert(uv.fs_mkstemp(test_dir .. '/stXXXXXX'))
        assert(type(fd) == 'number' and vim.startswith(tmp, test_dir .. '/st'))
        assert(uv.fs_write(fd, 'temp') == 4)
        assert(uv.fs_close(fd))

        -- rename / link / symlink / readlink / lstat
        assert(uv.fs_rename(tmp, test_dir .. '/renamed'))
        assert(uv.fs_link(test_dir .. '/renamed', test_dir .. '/hard'))
        assert(uv.fs_unlink(test_dir .. '/hard'))
        assert(uv.fs_symlink(test_dir .. '/renamed', test_dir .. '/soft', { dir = false }))
        assert(uv.fs_readlink(test_dir .. '/soft') == test_dir .. '/renamed')
        assert(assert(uv.fs_lstat(test_dir .. '/soft')).type == 'link')
        assert(assert(uv.fs_stat(test_dir .. '/soft')).type == 'file')
        assert(uv.fs_lutime(test_dir .. '/soft', 1000000001, 1000000001))
        assert(assert(uv.fs_lstat(test_dir .. '/soft')).mtime.sec == 1000000001)

        -- copyfile, with the exclusive flag failing on an existing destination
        assert(uv.fs_copyfile(test_dir .. '/renamed', test_dir .. '/copy'))
        assert(assert(uv.fs_stat(test_dir .. '/copy')).size == 4)
        local clash, _, clash_name = uv.fs_copyfile(test_dir .. '/copy', test_dir .. '/copy', { excl = true })
        assert(clash == nil and clash_name == 'EEXIST', clash_name)

        -- statfs
        local stats = assert(uv.fs_statfs(test_dir))
        assert(stats.bsize > 0 and stats.blocks > 0 and stats.bavail <= stats.blocks)

        -- sendfile
        local in_fd = assert(uv.fs_open(test_dir .. '/copy', 'r', 0))
        local out_fd = assert(uv.fs_open(test_dir .. '/sent', 'w', tonumber('644', 8)))
        assert(uv.fs_sendfile(out_fd, in_fd, 0, 4) == 4)
        assert(uv.fs_close(in_fd))
        assert(uv.fs_close(out_fd))
        assert(assert(uv.fs_stat(test_dir .. '/sent')).size == 4)
        ",
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn fs_failures_report_luv_shapes_sync_and_async() {
    let dir = fresh_dir("fail");
    let (host, scheduler) = host();
    host.lua()
        .globals()
        .set("test_dir", dir.to_string_lossy().as_ref())
        .unwrap();
    drive(
        &host,
        &scheduler,
        r"
        local uv = vim.uv

        -- Sync fail shape: nil, err, name
        local ok, err, name = uv.fs_stat(test_dir .. '/missing')
        assert(ok == nil and name == 'ENOENT' and type(err) == 'string')

        ok, err, name = uv.fs_open(test_dir .. '/missing', 'r', 0)
        assert(ok == nil and name == 'ENOENT')

        -- Unknown descriptor
        ok, err, name = uv.fs_close(4242)
        assert(ok == nil and name == 'EBADF')

        ok, err, name = uv.fs_read(4242, 4, 0)
        assert(ok == nil and name == 'EBADF')

        -- Invalid flags string raises, as luv's luaL_error does
        local raised = select(2, pcall(uv.fs_open, test_dir .. '/x', 'q', 0))
        assert(string.find(tostring(raised), 'invalid open flags'), tostring(raised))

        -- Async convention: error is the first and only leading argument
        async_stat_error = nil
        async_realpath_error = nil
        uv.fs_stat(test_dir .. '/missing', function(stat_error, stat)
          assert(stat == nil)
          async_stat_error = stat_error
        end)
        uv.fs_realpath(test_dir .. '/missing', function(path_error, resolved)
          assert(resolved == nil)
          async_realpath_error = path_error
        end)
        ",
    );
    let error = host
        .lua()
        .globals()
        .get::<String>("async_stat_error")
        .unwrap();
    assert!(error.contains("ENOENT"), "unexpected error string: {error}");
    let error = host
        .lua()
        .globals()
        .get::<String>("async_realpath_error")
        .unwrap();
    assert!(error.contains("ENOENT"), "unexpected error string: {error}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn fs_async_callbacks_receive_results_after_the_scheduler_drains() {
    let dir = fresh_dir("async");
    std::fs::write(dir.join("payload.bin"), b"round-trip").unwrap();
    let (host, scheduler) = host();
    host.lua()
        .globals()
        .set("test_dir", dir.to_string_lossy().as_ref())
        .unwrap();
    drive(
        &host,
        &scheduler,
        r"
        local uv = vim.uv
        async_result = ''
        uv.fs_open(test_dir .. '/payload.bin', 'r', 0, function(err, fd)
          assert(err == nil and type(fd) == 'number')
          uv.fs_fstat(fd, function(stat_err, stat)
            assert(stat_err == nil and stat.size == 10)
            uv.fs_read(fd, 10, 0, function(read_err, data)
              assert(read_err == nil and data == 'round-trip')
              uv.fs_close(fd, function(close_err, success)
                assert(close_err == nil and success == true)
                async_result = async_result .. 'done'
              end)
            end)
          end)
        end)
        uv.fs_scandir(test_dir, function(err, handle)
          assert(err == nil and handle ~= nil)
          local name = uv.fs_scandir_next(handle)
          assert(name == 'payload.bin', name)
          async_result = async_result .. 'scanned'
        end)
        ",
    );
    assert_eq!(
        host.lua().globals().get::<String>("async_result").unwrap(),
        "scanneddone"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn misc_surface_reports_process_and_system_state() {
    let (host, scheduler) = host();
    drive(
        &host,
        &scheduler,
        r"
        local uv = vim.uv
        assert(type(uv.cwd()) == 'string' and vim.startswith(uv.cwd(), '/'))
        assert(type(uv.os_tmpdir()) == 'string' and #uv.os_tmpdir() > 0)
        assert(type(uv.os_homedir()) == 'string' and #uv.os_homedir() > 0)
        assert(type(uv.exepath()) == 'string' and #uv.exepath() > 0)
        local uname = assert(uv.os_uname())
        assert(type(uname.sysname) == 'string' and type(uname.release) == 'string'
          and type(uname.version) == 'string' and type(uname.machine) == 'string')
        assert(uv.getpid() > 0 and uv.os_getpid() == uv.getpid())
        local before = uv.hrtime()
        assert(uv.hrtime() >= before)
        local sec, usec = uv.gettimeofday()
        assert(sec > 1500000000 and usec >= 0 and usec < 1000000)
        assert(type(uv.uptime()) == 'number' and uv.uptime() > 0)
        local one, five, fifteen = uv.loadavg()
        assert(one >= 0 and five >= 0 and fifteen >= 0)
        assert(uv.get_total_memory() > 0)
        assert(uv.get_free_memory() > 0 and uv.get_free_memory() <= uv.get_total_memory())
        assert(type(uv.os_getenv('PATH')) == 'string')
        local environment = uv.os_environ()
        assert(type(environment) == 'table' and environment.PATH == uv.os_getenv('PATH'))
        local missing, missing_err, missing_name = uv.os_getenv('OXVIM_UNSET_ENV_VAR_12345')
        assert(missing == nil and missing_name == 'ENOENT' and type(missing_err) == 'string')
        misc_ok = true
        ",
    );
    assert!(host.lua().globals().get::<bool>("misc_ok").unwrap());
}

#[test]
fn os_setenv_and_os_unsetenv_round_trip_through_the_process() {
    const NAME: &str = "OXVIM_UV_OS_SETENV_ROUNDTRIP";
    let _guard = EnvGuard::take(NAME);
    let (host, scheduler) = host();
    host.lua().globals().set("env_name", NAME).unwrap();
    drive(
        &host,
        &scheduler,
        r"
        local uv = vim.uv
        -- Set a fresh variable and read it back.
        assert(uv.os_setenv(env_name, 'first') == true)
        assert(uv.os_getenv(env_name) == 'first')
        -- Replacement wins over the previous value.
        assert(uv.os_setenv(env_name, 'second') == true)
        assert(uv.os_getenv(env_name) == 'second')
        -- os_environ enumerates the live process environment.
        assert(uv.os_environ()[env_name] == 'second')
        -- Unset removes it; a later read fails with ENOENT.
        assert(uv.os_unsetenv(env_name) == true)
        local value, err, name = uv.os_getenv(env_name)
        assert(value == nil and name == 'ENOENT' and type(err) == 'string')
        -- A name the platform refuses is EINVAL, not a silent success.
        local refused, refused_err, refused_name = uv.os_setenv('', 'x')
        assert(refused == nil and refused_name == 'EINVAL' and type(refused_err) == 'string')
        local refused_unset, unset_err, unset_name = uv.os_unsetenv('')
        assert(refused_unset == nil and unset_name == 'EINVAL' and type(unset_err) == 'string')
        ",
    );
}

#[test]
fn chdir_round_trips_through_the_process_working_directory() {
    let dir = fresh_dir("chdir");
    let (host, scheduler) = host();
    host.lua()
        .globals()
        .set("test_dir", dir.to_string_lossy().as_ref())
        .unwrap();
    drive(
        &host,
        &scheduler,
        r"
        local uv = vim.uv
        local before = assert(uv.cwd())
        assert(uv.chdir(test_dir) == 0)
        assert(assert(uv.cwd()) == test_dir)
        local ok, err, name = uv.chdir('/definitely/not/a/directory')
        assert(ok == nil and name == 'ENOENT')
        assert(uv.chdir(before) == 0)
        assert(assert(uv.cwd()) == before)
        chdir_ok = true
        ",
    );
    assert!(host.lua().globals().get::<bool>("chdir_ok").unwrap());
    std::fs::remove_dir_all(&dir).unwrap();
}
