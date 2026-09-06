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
