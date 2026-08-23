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
/// `-es` is `silent_mode`: `message.c` `msg_puts_printf` (line 3038) drops
/// message text while `'verbose'` is zero, but the informative listing
/// commands (`:print` through `print_line`, `:set` display through
/// `showoneopt`) clear `silent_mode` and write to stdout, so those are the
/// observable channel here.  Use [`batch_verbose`] to observe `:echo`.
fn batch(arguments: &[&str], input: &str) -> std::process::Output {
    spawn_batch(&["-es", "-u", "NONE", "-i", "NONE"], arguments, input)
}

/// Runs a silent Ex batch session with `'verbose'` at 1, which is what keeps
/// `msg_puts_printf` from dropping ordinary message output; it then reaches
/// stderr.
fn batch_verbose(arguments: &[&str], input: &str) -> std::process::Output {
    spawn_batch(&["-es", "-V1", "-u", "NONE", "-i", "NONE"], arguments, input)
}

fn spawn_batch(mode: &[&str], arguments: &[&str], input: &str) -> std::process::Output {
    let mut child = oxvim()
        .args(mode)
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

/// A scratch file holding `contents`, removed when the guard drops.
struct TempFile(std::path::PathBuf);

impl TempFile {
    fn new(suffix: &str, contents: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join(format!("oxvim-t57-{}-{unique}{suffix}", std::process::id()));
        std::fs::write(&path, contents).expect("write scratch file");
        Self(path)
    }

    fn text(&self) -> &str {
        self.0.to_str().expect("UTF-8 temp path")
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _removed = std::fs::remove_file(&self.0);
    }
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

/// `message.c` decides where message text goes, per message:
/// `msg_use_printf` (line 3013) prints when no UI or `--embed` peer can
/// display it, and `msg_puts_printf` then drops it while `silent_mode` is set
/// and `'verbose'` is zero (line 3038), else writes it to stderr (line 3049).
/// Each case below is byte-compared against `nvim` of the same build.
#[test]
fn message_output_follows_the_process_mode() {
    // --headless: no UI, not silent, so :echo reaches stderr.
    let headless = oxvim()
        .args(["-u", "NONE", "-i", "NONE", "--headless", "-c", "echo \"HELLO\"", "-c", "qall!"])
        .output()
        .expect("spawn oxvim");
    assert!(headless.status.success());
    assert_eq!(headless.stderr, b"HELLO", "--headless :echo belongs on stderr");
    assert!(headless.stdout.is_empty());

    // -es: silent_mode with 'verbose' zero drops the same message entirely.
    let silent = batch(&[], "echo \"HELLO\"\n");
    assert!(silent.status.success());
    assert!(silent.stdout.is_empty(), "-es :echo must not reach stdout");
    assert!(silent.stderr.is_empty(), "-es :echo must not reach stderr");

    // -es -V1: a nonzero 'verbose' defeats that suppression, and batch mode
    // ends its output with a newline (`ex_cmds.c` line 1721).
    let verbose = batch_verbose(&[], "echo \"HELLO\"\n");
    assert!(verbose.status.success());
    assert_eq!(verbose.stderr, b"HELLO\n");
    assert!(verbose.stdout.is_empty());
}

/// Informative listing output keeps its own stream: `print_line`
/// (`ex_cmds.c` line 1701) and `showoneopt` (`option.c` line 4851) clear
/// `silent_mode` and set `info_message`, so `:print` and `:set` display
/// survive `-es` on stdout while a neighbouring `:echo` is dropped, and a
/// message that follows them separates with a newline in their stream.
#[test]
fn informative_listings_keep_stdout_in_batch_mode() {
    let listing = batch(&[], "call setline(1, \"one\")\n%print\nset number?\necho \"gone\"\n");
    assert!(listing.status.success(), "{}", String::from_utf8_lossy(&listing.stderr));
    assert_eq!(String::from_utf8_lossy(&listing.stdout), "one\nnonumber\n");
    assert!(listing.stderr.is_empty(), "{}", String::from_utf8_lossy(&listing.stderr));

    // Under --headless nothing is silent, so the separator lands in the
    // stream of the message it follows: stderr after :echo, stdout between
    // two printed lines, and no trailing newline at exit.
    let headless = oxvim()
        .args([
            "-u", "NONE", "-i", "NONE", "--headless",
            "-c", "echo \"E\"",
            "-c", "set number?",
            "-c", "qall!",
        ])
        .output()
        .expect("spawn oxvim");
    assert!(headless.status.success());
    assert_eq!(headless.stdout, b"nonumber");
    assert_eq!(headless.stderr, b"E\n");
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
    let output = batch_verbose(
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
    // `:print` is informative listing output (stdout); `:echo` is an ordinary
    // message, which only escapes batch mode because `-V1` set 'verbose'.
    assert_eq!(String::from_utf8_lossy(&output.stdout), "one\ntwo\nthree\n");
    assert_eq!(String::from_utf8_lossy(&output.stderr), "first+second\n");
}

/// `-R` is upstream's `readonlymode` (`main.c` line 1286), and `open_buffer`
/// (`buffer.c` line 258) applies it to every buffer it loads for a named
/// file, not just the startup buffer. Windows padded with fresh empty
/// buffers stay writable, because upstream requires a file name.
#[test]
fn readonly_mode_reaches_every_loaded_startup_buffer() {
    let one = TempFile::new(".txt", "aaa\n");
    let two = TempFile::new(".txt", "bbb\n");
    let output = batch_verbose(
        &["-R", "-o5", one.text(), two.text()],
        "echo \"W1=\" . &readonly\n2wincmd w\necho \"W2=\" . &readonly\n4wincmd w\necho \"W4=\" . &readonly\n",
    );
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stderr), "W1=1\nW2=1\nW4=0\n");
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
        (vec!["--startuptime"], "Argument missing after: \"--startuptime\""),
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

/// `-R`, `-m`, `-M`, `-n` and `-b` reach the options they name before the
/// first startup command runs, and a cluster applies every letter in it.
#[test]
fn startup_option_flags_reach_their_options() {
    for (flags, query, expected) in [
        (vec!["-R"], "set readonly?", "readonly"),
        (vec!["-m"], "set write?", "nowrite"),
        (vec!["-M"], "set modifiable?", "nomodifiable"),
        (vec!["-n"], "set updatecount?", "updatecount=0"),
        (vec!["-b"], "set binary?", "binary"),
        // "-R" alone slows the swap file instead of disabling it.
        (vec!["-R"], "set updatecount?", "updatecount=10000"),
    ] {
        let output = batch(&flags, &format!("{query}\n"));
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{flags:?}: {}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(stdout.trim(), expected, "{flags:?}");
    }
    // A cluster is the same as the separate letters (main.c argv_idx).
    let output = batch(&["-Rnb"], "set readonly?\nset updatecount?\nset binary?\n");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "readonly\nupdatecount=0\nbinary\n"
    );
    // A startup command already observes them, because main.c sets them
    // during the scan.
    let output = batch(&["-R", "--cmd", "set readonly?"], "");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "readonly");
}

/// `-o`, `-O` and `-p` build the layout before the files load: an explicit
/// count wins, otherwise one window or tab page per file, never fewer than
/// the startup window, and the first window stays current.
#[test]
fn window_and_tab_openers_build_the_startup_layout() {
    let one = TempFile::new(".txt", "aaa\n");
    let two = TempFile::new(".txt", "bbb\n");
    let three = TempFile::new(".txt", "ccc\n");
    let files = [one.text(), two.text(), three.text()];
    for (flag, windows, tabs) in [
        ("-o", 3, 1),
        ("-o2", 2, 1),
        ("-o5", 5, 1),
        ("-O", 3, 1),
        ("-p", 1, 3),
        ("-p3", 1, 3),
        ("-p1", 1, 1),
    ] {
        let mut arguments = vec![flag];
        arguments.extend(files);
        let output = batch_verbose(&arguments, "echo winnr(\"$\") tabpagenr(\"$\") winnr()\n");
        assert!(output.status.success(), "{flag}: {}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(
            String::from_utf8_lossy(&output.stderr).trim(),
            format!("{windows} {tabs} 1"),
            "{flag}"
        );
    }
    // Every window shows the next file, in argv order (edit_buffers).
    let mut arguments = vec!["-o"];
    arguments.extend(files);
    let output = batch_verbose(&arguments, "echo bufname(\"%\")\n2wincmd w\necho bufname(\"%\")\n");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        format!("{}\n{}\n", one.text(), two.text())
    );
    // Without a layout flag there is still exactly one window.
    let mut arguments = vec!["--literal"];
    arguments.extend(files);
    let output = batch_verbose(&arguments, "echo winnr(\"$\") tabpagenr(\"$\")\n");
    assert_eq!(String::from_utf8_lossy(&output.stderr).trim(), "1 1");
}

/// `-E`/`-Es` set upstream's `input_istext`: standard input becomes buffer
/// text during startup, and the `+cmd` arguments then run over it.
#[test]
fn improved_ex_mode_reads_stdin_as_buffer_text() {
    let mut child = oxvim()
        .args(["-Es", "-u", "NONE", "-i", "NONE", "+%print"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn oxvim");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(b"hello\nworld\n")
        .expect("write text input");
    let output = child.wait_with_output().expect("wait for oxvim");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"hello\nworld\n");
}

/// A bare `-` edits standard input (upstream `EDIT_STDIN`), while `-` after
/// `-e` is the silent-mode modifier instead and leaves stdin as Ex input.
#[test]
fn bare_dash_edits_standard_input() {
    let written = TempFile::new(".txt", "");
    let mut child = oxvim()
        .args([
            "--headless",
            "-u",
            "NONE",
            "-i",
            "NONE",
            "-",
            "-c",
            &format!("w! {}", written.text()),
            "-c",
            "qa!",
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
        .write_all(b"from stdin\n")
        .expect("write stdin text");
    let output = child.wait_with_output().expect("wait for oxvim");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(std::fs::read_to_string(written.text()).expect("read written file"), "from stdin\n");

    // "-e -" is silent mode, so stdin stays Ex commands.
    let output = batch(&["-"], "call setline(1, \"ex input\")\n%print\n");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "ex input\n");
}

/// `--startuptime <file>` writes a timing log with one line per milestone.
#[test]
fn startuptime_writes_a_timing_log() {
    let log = TempFile::new(".log", "");
    let output = batch(&["--startuptime", log.text()], "");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let text = std::fs::read_to_string(log.text()).expect("read startuptime log");
    assert!(text.starts_with("--- Startup times for process:"), "{text}");
    for label in ["OXVIM STARTING", "parsing arguments", "opening buffers", "OXVIM STARTED"] {
        assert!(text.contains(label), "{label} missing from {text}");
    }
    // Without the flag nothing is written anywhere.
    let quiet = TempFile::new(".log", "");
    let output = batch(&[], "");
    assert!(output.status.success());
    assert_eq!(std::fs::read_to_string(quiet.text()).expect("read scratch"), "");
}

/// `-w{number}` sets `'window'`; the option is the whole observable effect
/// upstream produces at scan time.
#[test]
fn window_height_flag_sets_the_window_option() {
    let output = batch(&["-w42"], "set window?\n");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "window=42");
}

/// Upstream accepts these and does nothing with them, so accepting them is
/// parity rather than a silent no-op: `--literal` because file names are
/// always literal, `-N`/`-X`/`-f` because they are compatibility stubs, and
/// `-U {gvimrc}` because there is no GUI config to source.
#[test]
fn upstream_no_op_flags_are_accepted() {
    let gvimrc = TempFile::new(".vim", "throw 'never sourced'\n");
    let output = batch(
        &["--literal", "-N", "-X", "-f", "-U", gvimrc.text()],
        "call setline(1, \"ran\")\n%print\n",
    );
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "ran\n");
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
        (vec!["-w", "keys.log"], "script recording of typed keys"),
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
