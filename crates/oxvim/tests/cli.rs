#![allow(clippy::expect_used)]
//! Process-level coverage for non-interactive `oxvim` entry points.

use std::io::{Cursor, Write};
use std::process::{Command, Stdio};

use rmpv::Value;

fn oxvim() -> Command {
    Command::new(env!("CARGO_BIN_EXE_oxvim"))
}

fn map_field<'a>(value: &'a Value, name: &str) -> &'a Value {
    value
        .as_map()
        .and_then(|fields| fields.iter().find(|(key, _)| key.as_str() == Some(name)))
        .map(|(_, value)| value)
        .unwrap_or_else(|| panic!("missing metadata field {name}"))
}

#[test]
fn api_info_is_parseable_metadata() {
    let output = oxvim().arg("--api-info").output().expect("spawn oxvim");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let mut cursor = Cursor::new(&output.stdout);
    let metadata = rmpv::decode::read_value(&mut cursor).expect("decode api metadata");
    assert_eq!(cursor.position(), output.stdout.len() as u64, "trailing stdout bytes");
    assert_eq!(map_field(map_field(&metadata, "version"), "api_level").as_i64(), Some(15));
    assert!(!map_field(&metadata, "functions").as_array().expect("functions array").is_empty());
}

#[test]
fn silent_ex_pipeline_prints_buffer_and_quits() {
    let mut child = oxvim()
        .arg("-es")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn oxvim");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(b"call setline(1, \"x\") | %print | quit!\n")
        .expect("write Ex input");
    let output = child.wait_with_output().expect("wait for oxvim");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"x\n");
}

#[test]
fn lua_entry_receives_script_and_trailing_arguments() {
    let path = std::env::temp_dir().join(format!("oxvim-args-{}.lua", std::process::id()));
    std::fs::write(&path, "io.write(arg[0], '|', arg[1], '|', arg[2])")
        .expect("write Lua script");
    let output = oxvim()
        .args(["-l", path.to_str().expect("UTF-8 temp path"), "first", "--second"])
        .output()
        .expect("spawn oxvim");
    let _removed = std::fs::remove_file(&path);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout), format!("{}|first|--second", path.display()));
}

#[test]
fn lua_failure_and_unknown_option_exit_one() {
    let path = std::env::temp_dir().join(format!("oxvim-error-{}.lua", std::process::id()));
    std::fs::write(&path, "error('script failed')").expect("write Lua script");
    let lua = oxvim().args(["-l", path.to_str().expect("UTF-8 temp path")]).output().expect("spawn oxvim");
    let _removed = std::fs::remove_file(&path);
    assert_eq!(lua.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&lua.stderr).contains("script failed"));

    let usage = oxvim().arg("--unknown").output().expect("spawn oxvim");
    assert_eq!(usage.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&usage.stderr).contains("Unknown option: --unknown"));
}

#[test]
fn batch_executes_pre_commands_before_stdin_and_post_commands_after() {
    let mut child = oxvim()
        .args(["-es", "--cmd", "let g:pre=1", "+echo g:pre"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn oxvim");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(b"call setline(1, \"x\") | %print\n")
        .expect("write Ex input");
    let output = child.wait_with_output().expect("wait for oxvim");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"x\n1\n");
}

#[test]
fn bare_script_option_exits_with_usage_error() {
    let output = oxvim().arg("-s").output().expect("spawn oxvim");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Argument missing after: -s"));
}

#[test]
fn script_input_is_not_yet_wired() {
    let path = std::env::temp_dir().join(format!("oxvim-script-{}.in", std::process::id()));
    std::fs::write(&path, "j").expect("write script input");
    let output = oxvim()
        .args(["-s", path.to_str().expect("UTF-8 temp path")])
        .output()
        .expect("spawn oxvim");
    let _removed = std::fs::remove_file(&path);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("normal-mode script mode is not yet wired"));
}
