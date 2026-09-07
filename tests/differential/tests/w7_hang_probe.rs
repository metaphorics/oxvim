//! Regression probes for the W7 exit-hang class: `vim.ui_attach` end-to-end
//! event delivery and the T734 child-process exit shape.
use differential::{Embedded, OXVIM, binary};
use std::time::{Duration, Instant};

/// Runs one `nvim_exec_lua` call and returns the response's result slot.
#[expect(
    clippy::panic,
    reason = "probe failures must fail the test with the failing shape"
)]
fn exec_lua(nvim: &mut Embedded, id: i64, chunk: &str) -> rmpv::Value {
    let (response, _) = nvim
        .request(
            id,
            "nvim_exec_lua",
            vec![chunk.into(), rmpv::Value::Array(Vec::new())],
        )
        .unwrap_or_else(|message| panic!("exec_lua {chunk:?} transport failed: {message}"));
    match &response {
        rmpv::Value::Array(items) if items.len() == 4 => items[3].clone(),
        other => panic!("exec_lua response shape {other:?}"),
    }
}

#[test]
#[expect(clippy::expect_used, reason = "spawn failure must fail the probe")]
fn w7_probe_ui_attach_receives_msg_events() {
    let mut nvim = Embedded::spawn(&binary(OXVIM)).expect("spawn release oxvim --embed");
    let attached = exec_lua(
        &mut nvim,
        41,
        r"
        local ns = vim.api.nvim_create_namespace('probe')
        vim.g.hit = 'none'
        vim.ui_attach(ns, {ext_messages=true}, function(event, kind, ...)
          vim.g.hit = event .. ':' .. tostring(kind)
        end)
        return 'ok'
        ",
    );
    assert_eq!(attached, rmpv::Value::from("ok"), "attach chunk failed");
    // :echo produces a UI message (msg_show, kind "echo"); the server
    // redraws after nvim_command (a mutating method), delivering queued
    // chrome events to the attached callback.
    exec_lua(&mut nvim, 42, "vim.cmd [[echo 'hello ui']] return 'echoed'");
    let hit = exec_lua(&mut nvim, 43, "return vim.g.hit");
    assert_eq!(
        hit,
        rmpv::Value::from("msg_show:echo"),
        "callback did not observe msg_show"
    );
}

#[test]
#[expect(clippy::expect_used, reason = "spawn failure must fail the probe")]
fn w7_probe_t734_child_exit_shape() {
    use std::process::Command;
    let t0 = Instant::now();
    let status = Command::new(binary(OXVIM))
        .args([
            "-u",
            "NONE",
            "--headless",
            "--cmd",
            "lua ns = vim.api.nvim_create_namespace 'testspace'",
            "--cmd",
            "lua vim.ui_attach(ns, {ext_popupmenu=true}, function() end)",
            "--cmd",
            "quitall!",
        ])
        .output()
        .expect("spawn child");
    assert!(
        t0.elapsed() < Duration::from_secs(15),
        "T734 child hung (elapsed {:?})",
        t0.elapsed()
    );
    assert_eq!(
        status.status.code(),
        Some(0),
        "stderr: {:?}",
        String::from_utf8_lossy(&status.stderr)
    );
}

#[test]
#[expect(clippy::expect_used, reason = "spawn failure must fail the probe")]
fn w7_probe_ui_attach_single_delivery_with_rpc_ui() {
    let mut nvim = Embedded::spawn(&binary(OXVIM)).expect("spawn release oxvim --embed");
    // Attach an RPC UI first: the chrome snapshot replay bug only shows
    // when a remote UI keeps the wire path alive across redraws.
    nvim.request(
        40,
        "nvim_ui_attach",
        vec![80.into(), 24.into(), rmpv::Value::Map(Vec::new())],
    )
    .expect("ui attach");
    let attached = exec_lua(
        &mut nvim,
        41,
        r"
        local ns = vim.api.nvim_create_namespace('probe')
        vim.g.count = 0
        vim.ui_attach(ns, {ext_messages=true}, function(event, kind, ...)
          -- The probe environment can emit an E739 setup error before the
          -- callback registers; count only the echo-kind messages.
          if event == 'msg_show' and kind == 'echo' then
            vim.g.count = vim.g.count + 1
            vim.g.last = tostring(kind)
          end
        end)
        return 'ok'
        ",
    );
    assert_eq!(attached, rmpv::Value::from("ok"));
    // nvim_command (not vim.cmd-inside-exec_lua): the nested-command
    // truncate/re-push message lifecycle is a separate known drift.
    nvim.request(42, "nvim_command", vec!["echo 'one'".into()])
        .expect("echo");
    // Extra mutating calls force further redraws; a replaying snapshot
    // would deliver msg_show on each one (upstream: exactly once).
    exec_lua(
        &mut nvim,
        43,
        "vim.api.nvim_buf_set_lines(0, 0, -1, true, {'a'}) return 'w'",
    );
    exec_lua(
        &mut nvim,
        44,
        "vim.api.nvim_buf_set_lines(0, 0, -1, true, {'b'}) return 'w'",
    );
    let last = exec_lua(&mut nvim, 45, "return tostring(vim.g.last)");
    let count = exec_lua(&mut nvim, 45, "return vim.g.count");
    assert_eq!(
        count,
        rmpv::Value::from(1),
        "msg_show must deliver exactly once with an RPC UI attached (last: {last:?})"
    );
}
