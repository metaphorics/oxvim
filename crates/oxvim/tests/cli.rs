#![allow(clippy::expect_used)]
//! Process-level coverage for non-interactive `oxvim` entry points.

use std::io::{Cursor, Write};
use std::process::{Command, Stdio};

use rmpv::Value;

fn oxvim() -> Command {
    Command::new(env!("CARGO_BIN_EXE_oxvim"))
}


/// Runs a silent Ex batch session and returns its captured output.
///
/// `-es` is the only startup mode whose message stream reaches stdout today,
/// so it is the channel every observable-effect assertion below reads.
fn batch(arguments: &[&str], input: &str) -> std::process::Output {
    let mut child = oxvim()
        .args(["-es", "-u", "NONE", "-i", "NONE"])
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn oxvim");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(input.as_bytes())
        .expect("write Ex input");
    child.wait_with_output().expect("wait for oxvim")
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
fn cquit_and_qall_exit_codes_follow_ex_docmd() {
    // ex_cquit: no count means EXIT_FAILURE; a count is the status.
    let default = oxvim().args(["-u", "NONE", "--headless", "+cquit"]).output().expect("spawn oxvim");
    assert_eq!(default.status.code(), Some(1));
    let coded = oxvim().args(["-u", "NONE", "--headless", "+cquit 7"]).output().expect("spawn oxvim");
    assert_eq!(coded.status.code(), Some(7));

    // ex_quitall: clean buffers exit 0, modified ones raise E37 unless !.
    let clean = oxvim().args(["-u", "NONE", "--headless", "+qall"]).output().expect("spawn oxvim");
    assert_eq!(clean.status.code(), Some(0));
    let modified = oxvim()
        .args(["-u", "NONE", "--headless", "+call setline(1, 'x')", "+qall"])
        .output()
        .expect("spawn oxvim");
    assert_eq!(modified.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&modified.stderr).contains("E37"));
    let forced = oxvim()
        .args(["-u", "NONE", "--headless", "+call setline(1, 'x')", "+qall!"])
        .output()
        .expect("spawn oxvim");
    assert_eq!(forced.status.code(), Some(0));
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
    assert!(
        String::from_utf8_lossy(&usage.stderr)
            .contains("Unknown option argument: \"--unknown\"")
    );
}

/// `main.c` finishes startup, `-c`/`+cmd` included, before the Ex command
/// loop reads its first line of standard input.
#[test]
fn batch_runs_pre_and_post_commands_before_reading_stdin() {
    let mut child = oxvim()
        .args(["-es", "-u", "NONE", "--cmd", "call setline(1, \"PRE\")", "+call setline(2, \"PLUS\")"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn oxvim");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(b"%print\n")
        .expect("write Ex input");
    let output = child.wait_with_output().expect("wait for oxvim");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"PRE\nPLUS\n");
}

#[test]
fn bare_script_option_exits_with_usage_error() {
    let output = oxvim().arg("-s").output().expect("spawn oxvim");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Argument missing after: \"-s\""), "{stderr}");
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

#[test]
fn noplugin_resets_loadplugins_option() {
    let mut child = oxvim()
        .args(["-e", "-s", "--noplugin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn oxvim");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(b"set loadplugins?\n")
        .expect("write Ex input");
    let output = child.wait_with_output().expect("wait for oxvim");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("noloadplugins"));
}

#[test]
fn session_flag_sources_file_after_startup() {
    let path = std::env::temp_dir().join(format!("oxvim-session-{}.vim", std::process::id()));
    std::fs::write(&path, "call setline(1, \"sourced\")\n%print\n").expect("write session script");
    let mut child = oxvim()
        .args([
            "-e",
            "-s",
            "-u",
            "NONE",
            "--noplugin",
            "-S",
            path.to_str().expect("UTF-8 temp path"),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn oxvim");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(b"\n")
        .expect("write empty Ex input");
    let output = child.wait_with_output().expect("wait for oxvim");
    let _removed = std::fs::remove_file(&path);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("sourced"));
}


/// `-h`/`-?`/`--help` and `-v`/`--version` print and exit 0, upstream's
/// `usage()`/`version()` followed by `os_exit(0)`.  The version text must
/// carry the version and the API level; a script reads the level to decide
/// which RPC surface exists.
#[test]
fn help_and_version_print_and_exit_zero() {
    for flag in ["-h", "-?", "--help", "--HELP"] {
        let output = oxvim().arg(flag).output().expect("spawn oxvim");
        assert_eq!(output.status.code(), Some(0), "{flag}");
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.starts_with("Usage:\n"), "{flag}: {text}");
        assert!(text.contains("--cmd <cmd>"), "{flag}: {text}");
        assert!(output.stderr.is_empty(), "{flag}");
    }
    for flag in ["-v", "--version"] {
        let output = oxvim().arg(flag).output().expect("spawn oxvim");
        assert_eq!(output.status.code(), Some(0), "{flag}");
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.starts_with("OXVIM v"), "{flag}: {text}");
        assert!(text.contains("API level 15"), "{flag}: {text}");
    }
    // main.c prints from inside the scan, so a later bad option never runs.
    let output = oxvim().args(["--version", "--bogus"]).output().expect("spawn oxvim");
    assert_eq!(output.status.code(), Some(0));
}

/// Upstream keeps `+cmd` and `-c` in one array and runs `--cmd` first, so the
/// post-startup commands stay in argv order regardless of how they spell.
#[test]
fn post_commands_keep_argv_order_after_every_pre_command() {
    let output = batch(
        &[
            "-c",
            "call setline(1, \"one\")",
            "--cmd",
            "let g:pre = \"first\"",
            "+call setline(2, \"two\")",
            "-ccall setline(3, \"three\")",
            "--cmd",
            "let g:pre = g:pre . \"+second\"",
        ],
        "%print\necho g:pre\n",
    );
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "one\ntwo\nthree\nfirst+second\n");
}

/// Every usage failure is observable: `mainerr` prints `{prog}: {msg}` with a
/// `-h` pointer and exits 1, and a repeated script file exits 2.
#[test]
fn usage_failures_match_upstream_text_and_status() {
    for (arguments, message) in [
        (vec!["--bogus"], "Unknown option argument: \"--bogus\""),
        (vec!["-Q"], "Unknown option argument: \"-Q\""),
        (vec!["-uxx", "NONE"], "Garbage after option argument: \"-uxx\""),
        (vec!["--cmdfoo", "x"], "Garbage after option argument: \"--cmdfoo\""),
        (vec!["-u"], "Argument missing after: \"-u\""),
        (vec!["-c"], "Argument missing after: \"-c\""),
    ] {
        let output = oxvim().args(&arguments).output().expect("spawn oxvim");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}: {stderr}");
        assert!(stderr.contains(message), "{arguments:?}: {stderr}");
        assert!(stderr.contains("More info with \"oxvim -h\""), "{arguments:?}: {stderr}");
    }
    // scripterror: a bare line and status 2, with no "-h" pointer.
    let output = oxvim().args(["-s", "a", "-s", "b"]).output().expect("spawn oxvim");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("Attempt to open script file again: \"-s b\""), "{stderr}");
    assert!(!stderr.contains("More info"), "{stderr}");
}

/// A flag whose honest behavior needs a subsystem oxvim does not have is
/// rejected, not accepted and ignored: a caller can detect the status and the
/// named requirement, but could never detect a silent no-op.
#[test]
fn flags_without_their_subsystem_are_rejected_by_name() {
    for (arguments, requirement) in [
        (vec!["-d"], "a diff engine"),
        (vec!["-A"], "the 'arabic' option side effects and keymap files"),
        (vec!["-H"], "keymap file loading"),
        (vec!["-D"], "the Ex debugger"),
        (vec!["-q", "errors.err"], "the quickfix list and 'errorformat'"),
        (vec!["-t", "sometag"], "the tags subsystem"),
        (vec!["-r"], "swap-file recovery"),
        (vec!["-L"], "swap-file recovery"),
        (vec!["-W", "keys.log"], "script recording of typed keys"),
        (vec!["--remote", "file"], "RPC client channels and vim._cs_remote"),
        (vec!["--remote-send", "iabc"], "RPC client channels and vim._cs_remote"),
        (vec!["--server", "127.0.0.1:1"], "RPC client channels and vim._cs_remote"),
        (vec!["--luamod-dev"], "the Lua module preload table"),
    ] {
        let output = oxvim().args(&arguments).output().expect("spawn oxvim");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}: {stderr}");
        assert!(stderr.contains("Option not supported"), "{arguments:?}: {stderr}");
        assert!(stderr.contains(requirement), "{arguments:?}: {stderr}");
    }
}
