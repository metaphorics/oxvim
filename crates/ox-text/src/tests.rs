use std::fs;
use std::io::Cursor as IoCursor;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rmpv::Value;

use super::*;

fn bytes(lines: &[&str]) -> Vec<Vec<u8>> {
    lines.iter().map(|line| line.as_bytes().to_vec()).collect()
}

#[test]
fn buffer_preserves_newline_contract_and_tick() {
    let mut buffer = Buffer::from_bytes(b"alpha\nbeta\n").unwrap();
    assert_eq!(buffer.line_count(), 2);
    assert!(buffer.has_eol());
    assert_eq!(buffer.line(2).unwrap(), b"beta");
    assert_eq!(buffer.byte_of_line(2).unwrap(), 6);
    assert_eq!(buffer.lnum_of_byte(6).unwrap(), 2);
    assert_eq!(buffer.lnum_of_byte(11).unwrap(), 2);

    buffer.replace_lines(2, 2, &bytes(&["B", "C"])).unwrap();
    assert_eq!(buffer.changedtick(), 1);
    assert_eq!(buffer.to_bytes(), b"alpha\nB\nC\n");
    buffer.append_lines(0, &bytes(&["zero"])).unwrap();
    assert_eq!(buffer.changedtick(), 2);
    buffer.delete_lines(1, 1).unwrap();
    assert_eq!(buffer.changedtick(), 3);
}

#[test]
fn randomized_buffer_matches_vec_model() {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut buffer = Buffer::new();
    let mut model = vec![Vec::new()];
    for expected_tick in 1..=600_u64 {
        state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        let start = usize::try_from(state >> 32).unwrap() % model.len();
        state = state.rotate_left(17).wrapping_mul(0xd134_2543_de82_ef95);
        let span = usize::try_from(state >> 32).unwrap() % (model.len() - start) + 1;
        let replacement_count = usize::try_from(state & 3).unwrap();
        let mut replacement = Vec::new();
        for index in 0..replacement_count {
            state = state.rotate_left(9).wrapping_add(0xa076_1d64_78bd_642f);
            replacement.push(format!("{state:016x}-{index}").into_bytes());
        }
        buffer
            .replace_lines(start + 1, start + span, &replacement)
            .unwrap();
        model.splice(start..start + span, replacement);
        if model.is_empty() {
            model.push(Vec::new());
        }
        assert_eq!(buffer.changedtick(), expected_tick);
        assert_eq!(buffer.line_count(), model.len());
        let mut offset = 0;
        for (index, expected) in model.iter().enumerate() {
            assert_eq!(buffer.line(index + 1).unwrap(), *expected);
            assert_eq!(buffer.byte_of_line(index + 1).unwrap(), offset);
            offset += expected.len() + usize::from(index + 1 != model.len() || buffer.has_eol());
        }
    }
}

#[test]
fn marks_follow_splice_boundaries() {
    let mut marks = Marks::new();
    marks.set(1, Position { lnum: 1, col: 4 });
    marks.set(2, Position { lnum: 2, col: 3 });
    marks.set(3, Position { lnum: 3, col: 8 });
    marks.set(4, Position { lnum: 4, col: 1 });
    marks.set(5, Position { lnum: 5, col: 2 });
    marks.splice(2, 3, 2);
    assert_eq!(marks.get(1), Some(Position { lnum: 1, col: 4 }));
    assert_eq!(marks.get(2), Some(Position { lnum: 2, col: 3 }));
    assert_eq!(marks.get(3), Some(Position { lnum: 3, col: 8 }));
    assert_eq!(marks.get(4), Some(Position { lnum: 2, col: 0 }));
    assert_eq!(marks.get(5), Some(Position { lnum: 4, col: 2 }));
}

fn edit(label: &str) -> LineEdit {
    LineEdit {
        start: 1,
        before: bytes(&[""]),
        after: bytes(&[label]),
        cursor_before: Cursor::default(),
        cursor_after: Cursor { lnum: 1, col: label.len() },
    }
}

#[test]
fn undo_tree_preserves_and_navigates_branches() {
    let mut tree = UndoTree::new();
    let first = tree.record(edit("one"), 10);
    let second = tree.record(edit("two"), 20);
    assert_eq!(tree.undo().unwrap(), UndoStep::Undo(UndoEntry { seq: second, timestamp: 20, edit: edit("two") }));
    let third = tree.record(edit("branch"), 30);
    assert_eq!(tree.current_seq(), third);
    tree.undo().unwrap();
    assert_eq!(tree.branches(), vec![second, third]);
    assert!(matches!(tree.redo_branch(0).unwrap(), UndoStep::Redo(entry) if entry.seq == second));
    let steps = tree.undo_to_seq(third).unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(tree.current_seq(), third);
    assert_eq!(tree.undo_to_seq(first).unwrap().len(), 1);
}

#[test]
fn undo_file_hash_and_empty_format_round_trip() {
    let buffer = Buffer::from_bytes(b"one\ntwo\n").unwrap();
    let undo = UndoFile::empty_for_buffer(&buffer);
    undo.verify_buffer(&buffer).unwrap();
    let mut encoded = Vec::new();
    undo.write(&mut encoded).unwrap();
    let decoded = UndoFile::read(IoCursor::new(encoded)).unwrap();
    decoded.verify_buffer(&buffer).unwrap();
    assert_eq!(decoded.line_count(), 2);
}

#[test]
fn swap_snapshot_block_round_trip() {
    let buffer = Buffer::from_bytes(b"one\ntwo\nthree\n").unwrap();
    let swap = SwapFile::new("/tmp/example.txt", buffer);
    let mut encoded = Vec::new();
    swap.write(&mut encoded).unwrap();
    let decoded = SwapFile::read(IoCursor::new(encoded)).unwrap();
    assert_eq!(decoded.file_name, "/tmp/example.txt");
    assert_eq!(decoded.buffer.to_bytes(), b"one\ntwo\nthree\n");
}

#[test]
fn shada_round_trip_size_limit_and_merge() {
    let old = ShaDaEntry::new(
        ShaDaEntryType::Register,
        10,
        Value::Map(vec![(Value::from("n"), Value::from(u64::from(b'a'))), (Value::from("rc"), Value::Array(vec![Value::Binary(b"old".to_vec())]))]),
    );
    let new = ShaDaEntry { timestamp: 20, data: Value::Map(vec![(Value::from("n"), Value::from(u64::from(b'a'))), (Value::from("rc"), Value::Array(vec![Value::Binary(b"new".to_vec())]))]), ..old.clone() };
    let merged = ShaDa { entries: vec![old] }.merge(&ShaDa { entries: vec![new.clone()] });
    assert_eq!(merged.entries, vec![new]);

    let histories = ShaDa {
        entries: vec![
            ShaDaEntry::new(
                ShaDaEntryType::History,
                1,
                Value::Array(vec![Value::from(0), Value::Binary(b"same".to_vec())]),
            ),
            ShaDaEntry::new(
                ShaDaEntryType::History,
                2,
                Value::Array(vec![Value::from(1), Value::Binary(b"same".to_vec())]),
            ),
        ],
    };
    assert_eq!(histories.merge(&ShaDa::default()).entries.len(), 2);

    let stream = ShaDa { entries: vec![ShaDaEntry::new(ShaDaEntryType::Header, 1, Value::Map(vec![])), ShaDaEntry::new(ShaDaEntryType::SubString, 2, Value::Array(vec![Value::Binary(vec![b'x'; 2048])]))] };
    let mut limited = Vec::new();
    stream.write(&mut limited, 1).unwrap();
    let decoded = ShaDa::read(IoCursor::new(limited), 1).unwrap();
    assert_eq!(decoded.entries.len(), 1);
    assert_eq!(decoded.entries[0].kind(), Some(ShaDaEntryType::Header));
}

fn oracle_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let path = std::env::temp_dir().join(format!("ox-text-{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn nvim() -> &'static str {
    "/home/alpha/rewrite/Oxvim/.references/neovim/build/bin/nvim"
}

fn run_nvim(args: &[String]) {
    let output = Command::new(nvim()).args(args).output().unwrap();
    assert!(output.status.success(), "nvim failed: {}", String::from_utf8_lossy(&output.stderr));
}

fn command_arg(command: impl AsRef<str>) -> String {
    command.as_ref().to_owned()
}

#[test]
fn real_nvim_undo_and_shada_interoperate_both_directions() {
    if !Path::new(nvim()).exists() {
        return;
    }
    let dir = oracle_dir("formats");
    let text = dir.join("text.txt");
    let upstream_undo = dir.join("upstream.un~");
    let emitted_undo = dir.join("emitted.un~");
    fs::write(&text, b"one\ntwo\n").unwrap();
    run_nvim(&[
        "--headless".into(), "-u".into(), "NONE".into(), "-n".into(), text.display().to_string(),
        "-c".into(), command_arg("normal GoTHREE"),
        "-c".into(), command_arg("write"),
        "-c".into(), command_arg(format!("wundo! {}", upstream_undo.display())),
        "-c".into(), "qa!".into(),
    ]);
    let current = Buffer::from_bytes(&fs::read(&text).unwrap()).unwrap();
    let upstream = UndoFile::read(IoCursor::new(fs::read(&upstream_undo).unwrap())).unwrap();
    upstream.verify_buffer(&current).unwrap();
    let mut undo_bytes = Vec::new();
    upstream.write(&mut undo_bytes).unwrap();
    fs::write(&emitted_undo, undo_bytes).unwrap();
    let undo_result = dir.join("undo-result");
    run_nvim(&[
        "--headless".into(), "-u".into(), "NONE".into(), "-n".into(), text.display().to_string(),
        "-c".into(), command_arg(format!("rundo {}", emitted_undo.display())),
        "-c".into(), "undo".into(),
        "-c".into(), command_arg(format!("call writefile(getline(1, '$'), '{}')", undo_result.display())),
        "-c".into(), "qa!".into(),
    ]);
    assert_eq!(fs::read_to_string(&undo_result).unwrap(), "one\ntwo\n");

    let upstream_shada = dir.join("upstream.shada");
    run_nvim(&[
        "--headless".into(), "-u".into(), "NONE".into(), "-i".into(), upstream_shada.display().to_string(),
        "-c".into(), "let @a='oracle'".into(), "-c".into(), "wshada!".into(), "-c".into(), "qa!".into(),
    ]);
    let parsed = ShaDa::read(IoCursor::new(fs::read(&upstream_shada).unwrap()), 0).unwrap();
    let emitted_shada = dir.join("emitted.shada");
    let mut shada_bytes = Vec::new();
    parsed.write(&mut shada_bytes, 0).unwrap();
    fs::write(&emitted_shada, shada_bytes).unwrap();
    let register_result = dir.join("register-result");
    run_nvim(&[
        "--headless".into(), "-u".into(), "NONE".into(), "-i".into(), emitted_shada.display().to_string(),
        "-c".into(), command_arg(format!("call writefile([getreg('a')], '{}')", register_result.display())),
        "-c".into(), "qa!".into(),
    ]);
    assert_eq!(fs::read_to_string(&register_result).unwrap(), "oracle\n");

    let own_shada = dir.join("own.shada");
    let own = ShaDa { entries: vec![
        ShaDaEntry::new(ShaDaEntryType::Header, 1, Value::Map(vec![(Value::from("generator"), Value::Binary(b"ox-text".to_vec()))])),
        ShaDaEntry::new(ShaDaEntryType::Register, 2, Value::Map(vec![(Value::from("rc"), Value::Array(vec![Value::Binary(b"from-ox".to_vec())])), (Value::from("n"), Value::from(u64::from(b'a')))])),
    ] };
    let mut own_bytes = Vec::new();
    own.write(&mut own_bytes, 0).unwrap();
    fs::write(&own_shada, own_bytes).unwrap();
    let own_result = dir.join("own-result");
    run_nvim(&[
        "--headless".into(), "-u".into(), "NONE".into(), "-i".into(), own_shada.display().to_string(),
        "-c".into(), command_arg(format!("call writefile([getreg('a')], '{}')", own_result.display())),
        "-c".into(), "qa!".into(),
    ]);
    assert_eq!(fs::read_to_string(&own_result).unwrap(), "from-ox\n");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn real_nvim_swap_interoperates_both_directions() {
    if !Path::new(nvim()).exists() {
        return;
    }
    let dir = oracle_dir("swap");
    let text = dir.join("swap-text.txt");
    let marker = dir.join("swap-name");
    fs::write(&text, b"red\ngreen\nblue\n").unwrap();
    let directory_option = format!("set directory={}//", dir.display());
    let marker_command = format!("call writefile([swapname(bufnr())], '{}')", marker.display());
    let mut child = Command::new(nvim())
        .args(["--headless", "-u", "NONE", "--cmd", &directory_option, "--cmd", "set swapfile", text.to_str().unwrap(), "-c", "normal GoORANGE", "-c", "preserve", "-c", &marker_command, "-c", "sleep 10"])
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    for _ in 0..100 {
        if marker.exists() { break; }
        thread::sleep(Duration::from_millis(25));
    }
    let swap_name = fs::read_to_string(&marker).unwrap();
    let upstream_path = PathBuf::from(swap_name.trim());
    let copied = dir.join("upstream-copy.swp");
    fs::copy(&upstream_path, &copied).unwrap();
    child.kill().unwrap();
    let _ = child.wait();

    let upstream = SwapFile::read(IoCursor::new(fs::read(&copied).unwrap())).unwrap();
    assert_eq!(upstream.buffer.to_bytes(), b"red\ngreen\nblue\nORANGE\n");
    let emitted = dir.join("emitted.swp");
    let mut swap_bytes = Vec::new();
    upstream.write(&mut swap_bytes).unwrap();
    fs::write(&emitted, swap_bytes).unwrap();
    let recovered = dir.join("recovered");
    run_nvim(&[
        "--headless".into(), "-u".into(), "NONE".into(), "-n".into(), "-r".into(), emitted.display().to_string(),
        "-c".into(), command_arg(format!("call writefile(getline(1, '$'), '{}')", recovered.display())),
        "-c".into(), "qa!".into(),
    ]);
    assert_eq!(fs::read_to_string(recovered).unwrap(), "red\ngreen\nblue\nORANGE\n");
    fs::remove_dir_all(dir).unwrap();
}
