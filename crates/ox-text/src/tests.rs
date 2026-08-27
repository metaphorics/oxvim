use std::fs;
use std::io::Cursor as IoCursor;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rmpv::Value;

use super::*;
use crate::buffer::LineSplice;

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
fn byte_of_line_accepts_eof_pseudo_line_and_noeol() {
    let eol = Buffer::from_bytes(b"a\nb\n").unwrap();
    assert_eq!(eol.line_count(), 2);
    // EOF pseudo-line (line_count + 1) maps to the full serialized length.
    assert_eq!(eol.byte_of_line(3).unwrap(), 4);
    assert_eq!(eol.to_bytes().len(), 4);
    assert_eq!(
        eol.byte_of_line(4).unwrap_err(),
        BufferError::LineRange { start: 4, end: 4, line_count: 2 }
    );

    let noeol = Buffer::from_lines(&bytes(&["a", "b"]), false).unwrap();
    assert_eq!(noeol.line_count(), 2);
    assert!(!noeol.has_eol());
    // "a\nb": the absent final terminator is excluded from the EOF length.
    assert_eq!(noeol.byte_of_line(3).unwrap(), 3);
    assert_eq!(noeol.to_bytes(), b"a\nb");
    assert_eq!(
        noeol.byte_of_line(0).unwrap_err(),
        BufferError::LineRange { start: 0, end: 0, line_count: 2 }
    );
}

#[test]
fn lnum_of_byte_classifies_mid_char_offsets() {
    let buffer = Buffer::from_bytes("é\nx".as_bytes()).unwrap();
    assert_eq!(buffer.line_count(), 2);
    // Offsets 1 and 2 split the "é" code point but both live in line 1.
    assert_eq!(buffer.lnum_of_byte(1).unwrap(), 1);
    assert_eq!(buffer.lnum_of_byte(2).unwrap(), 1);
    assert_eq!(buffer.lnum_of_byte(3).unwrap(), 2); // the "\n"
    assert_eq!(buffer.lnum_of_byte(4).unwrap(), 2); // "x"

    let two_line = Buffer::from_bytes("a\né".as_bytes()).unwrap();
    assert_eq!(two_line.lnum_of_byte(3).unwrap(), 2); // mid "é"
    assert_eq!(two_line.lnum_of_byte(4).unwrap(), 2);
}

#[test]
fn undo_file_read_rejects_trailing_and_truncated_tails() {
    let buffer = Buffer::from_bytes(b"a\nb\n").unwrap();
    let mut tree = UndoTree::new();
    tree.record(edit_at(2, &bytes(&["b"]), &bytes(&["c"])), 1710000001);
    let undo = UndoFile::from_tree(&buffer, &tree);
    let mut valid = Vec::new();
    undo.write(&mut valid).unwrap();
    assert!(UndoFile::read(IoCursor::new(valid.clone())).is_ok());

    // A valid prefix plus two garbage bytes must be rejected.
    let mut garbage = valid.clone();
    garbage.extend_from_slice(b"ab");
    assert!(matches!(
        UndoFile::read(IoCursor::new(garbage)),
        Err(UndoFileError::Malformed)
    ));

    // Every truncation point must be rejected.
    for cut in [0, 9, 10, 11, valid.len() - 3, valid.len() - 1] {
        assert!(
            UndoFile::read(IoCursor::new(&valid[..cut])).is_err(),
            "truncated at {cut} accepted"
        );
    }

    // The empty-history file also rejects a trailing tail.
    let mut empty_bytes = Vec::new();
    UndoFile::empty_for_buffer(&buffer)
        .write(&mut empty_bytes)
        .unwrap();
    let mut polluted = empty_bytes;
    polluted.extend_from_slice(b"\0\0");
    assert!(matches!(
        UndoFile::read(IoCursor::new(polluted)),
        Err(UndoFileError::Malformed)
    ));
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

/// Differential oracle for the in-place ranged mutators. Unlike
/// [`randomized_buffer_matches_vec_model`] this also drives `append_lines`,
/// `delete_lines`, and `set_eol`, so every branch of the rope splice (interior
/// insert, append past a terminated and an unterminated final line, deletion
/// through the final line, and total deletion) is compared against a `Vec`
/// model that tracks end-of-line state alongside the lines.
#[test]
fn randomized_mixed_mutations_match_vec_model() {
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        usize::try_from(state >> 33).unwrap()
    };
    let mut buffer = Buffer::new();
    let mut model = vec![Vec::new()];
    let mut has_eol = false;
    for expected_tick in 1..=1200_u64 {
        let mut replacement = Vec::new();
        for index in 0..next() % 3 {
            replacement.push(format!("{}-{index}", next()).into_bytes());
        }
        match next() % 4 {
            0 => {
                let lnum = next() % (model.len() + 1);
                buffer.append_lines(lnum, &replacement).unwrap();
                model.splice(lnum..lnum, replacement);
            }
            1 => {
                let start = next() % model.len();
                let span = next() % (model.len() - start) + 1;
                buffer.delete_lines(start + 1, start + span).unwrap();
                model.drain(start..start + span);
            }
            2 => {
                let start = next() % model.len();
                let span = next() % (model.len() - start) + 1;
                buffer
                    .replace_lines(start + 1, start + span, &replacement)
                    .unwrap();
                model.splice(start..start + span, replacement);
            }
            _ => {
                has_eol = next() % 2 == 1;
                buffer.set_eol(has_eol);
            }
        }
        if model.is_empty() {
            // Deleting every line leaves the canonical empty Vim buffer.
            model.push(Vec::new());
            has_eol = false;
        }
        assert_eq!(buffer.changedtick(), expected_tick);
        assert_eq!(buffer.has_eol(), has_eol, "eol at tick {expected_tick}");
        assert_eq!(buffer.line_count(), model.len(), "count at tick {expected_tick}");
        let mut serialized = model.join(&b'\n');
        if has_eol {
            serialized.push(b'\n');
        }
        assert_eq!(buffer.to_bytes(), serialized, "bytes at tick {expected_tick}");
        let mut offset = 0;
        for (index, expected) in model.iter().enumerate() {
            assert_eq!(buffer.line(index + 1).unwrap(), *expected);
            assert_eq!(buffer.byte_of_line(index + 1).unwrap(), offset);
            offset += expected.len() + usize::from(index + 1 != model.len() || has_eol);
        }
        assert_eq!(buffer.byte_of_line(model.len() + 1).unwrap(), offset);
    }
}

/// Appending one line at a time must cost the edit, not the buffer.
///
/// The superseded implementation materialized every logical line into a `Vec`
/// and rebuilt the entire rope on each call, making an N-line insert O(N^2);
/// this loop measured 497.3s against it (a debug build on this workstation)
/// and pushed upstream's `test_window_cmd.vim` past a 120s timeout. The ranged
/// rope splice runs the same loop in 84ms. The one-second bound leaves an
/// order of magnitude of headroom over that, so a loaded machine cannot make
/// it flaky, yet it still fails hard against any return of the quadratic shape.
#[test]
fn appending_ten_thousand_lines_costs_the_edit_not_the_buffer() {
    let mut buffer = Buffer::new();
    let started = Instant::now();
    for index in 0..10_000_usize {
        let last = buffer.line_count();
        buffer
            .append_lines(last, &[format!("line {index}").into_bytes()])
            .unwrap();
    }
    let elapsed = started.elapsed();
    assert_eq!(buffer.line_count(), 10_001);
    assert_eq!(buffer.line(1).unwrap(), b"");
    assert_eq!(buffer.line(10_001).unwrap(), b"line 9999");
    assert!(
        elapsed < Duration::from_secs(1),
        "10000 single-line appends took {elapsed:?}, expected well under 1s"
    );
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

/// Records `edit` as its own undo block: the tree only starts a new header
/// when the previous one is closed, so a test about *branches* has to close
/// each one the way a typed key would.
fn record_block(tree: &mut UndoTree, label: &str, timestamp: i64) -> u64 {
    tree.sync();
    tree.record(edit(label), timestamp)
}

#[test]
fn undo_tree_preserves_and_navigates_branches() {
    let mut tree = UndoTree::new();
    let first = record_block(&mut tree, "one", 10);
    let second = record_block(&mut tree, "two", 20);
    assert_eq!(
        tree.undo().unwrap(),
        UndoStep::Undo(UndoEntry { seq: second, timestamp: 20, edits: vec![edit("two")] })
    );
    let third = record_block(&mut tree, "branch", 30);
    assert_eq!(tree.current_seq(), third);
    tree.undo().unwrap();
    assert_eq!(tree.branches(), vec![second, third]);
    assert!(matches!(tree.redo_branch(0).unwrap(), UndoStep::Redo(entry) if entry.seq == second));
    let steps = tree.undo_to_seq(third).unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(tree.current_seq(), third);
    assert_eq!(tree.undo_to_seq(first).unwrap().len(), 1);
}

/// Edits recorded without a sync between them are one header, and one undo
/// step takes the whole block back (`u_savecommon`, `undo.c:388-500`).
#[test]
fn unsynced_edits_join_one_undo_block() {
    let mut tree = UndoTree::new();
    let first = tree.record(edit("one"), 10);
    let joined = tree.record(edit("two"), 20);
    let also_joined = tree.record(edit("three"), 30);
    assert_eq!(joined, first, "an open block must not allocate a sequence");
    assert_eq!(also_joined, first);
    assert_eq!(tree.current_seq(), first);
    assert_eq!(tree.current_block_len(), 3);
    assert!(!tree.is_synced());
    assert_eq!(tree.summary().seq_last, first, "no extra header was created");

    let UndoStep::Undo(entry) = tree.undo().unwrap() else {
        panic!("expected an undo step");
    };
    assert_eq!(entry.edits, vec![edit("one"), edit("two"), edit("three")]);
    assert!(tree.is_synced(), "undoing closes the block");
    assert_eq!(tree.current_seq(), 0);
    assert!(tree.undo().is_err(), "one block, one step");
}

/// A sync between two edits makes two headers, which is the boundary a typed
/// key installs (`may_sync_undo`, `input.c:1300`).
#[test]
fn a_sync_between_edits_starts_a_new_undo_block() {
    let mut tree = UndoTree::new();
    let first = tree.record(edit("one"), 10);
    tree.sync();
    let second = tree.record(edit("two"), 20);
    assert_ne!(second, first);
    assert_eq!(tree.summary().seq_last, second);
    assert_eq!(tree.current_block_len(), 1);
    tree.undo().unwrap();
    assert_eq!(tree.current_seq(), first);
    tree.undo().unwrap();
    assert_eq!(tree.current_seq(), 0);
}

/// `:undojoin` reopens the newest block, is a no-op when one is already open,
/// and is `E790` after an undo (`ex_undojoin`, `undo.c:2800-2816`).
#[test]
fn undojoin_reopens_the_newest_block_but_never_after_an_undo() {
    let mut tree = UndoTree::new();
    // Nothing recorded yet: silent no-op, and the next edit still starts a
    // block of its own.
    tree.undojoin().unwrap();
    let first = tree.record(edit("one"), 10);
    // Already open: also a no-op, and the flag stays open.
    tree.undojoin().unwrap();
    assert!(!tree.is_synced());

    tree.sync();
    tree.undojoin().unwrap();
    let joined = tree.record(edit("two"), 20);
    assert_eq!(joined, first, "undojoin put the edit in the existing header");
    assert_eq!(tree.current_block_len(), 2);

    tree.undo().unwrap();
    assert_eq!(tree.undojoin(), Err(UndoError::JoinAfterUndo));

    // The other rejecting shape: an undo that stopped on an earlier header
    // rather than at the original state.
    let mut tree = UndoTree::new();
    record_block(&mut tree, "one", 10);
    record_block(&mut tree, "two", 20);
    tree.undo().unwrap();
    assert_ne!(tree.current_seq(), 0, "stopped on a header, not at the root");
    assert_eq!(tree.undojoin(), Err(UndoError::JoinAfterUndo));
}

/// The header list `undotree()` reports: oldest first along the active
/// branch, with the abandoned branch nested under `alt`
/// (`u_eval_tree`, `undo.c:3193-3221`).
#[test]
fn undotree_entries_report_the_active_branch_and_its_alternates() {
    let mut tree = UndoTree::new();
    let first = record_block(&mut tree, "one", 10);
    let second = record_block(&mut tree, "two", 20);
    tree.undo().unwrap();
    let branch = record_block(&mut tree, "branch", 30);

    let entries = tree.entries();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].seq, first);
    assert!(entries[0].alt.is_empty());
    assert_eq!(entries[1].seq, branch);
    assert!(entries[1].newhead, "the branch tip is b_u_newhead");
    assert!(!entries[1].curhead);
    assert_eq!(entries[1].alt.len(), 1);
    assert_eq!(entries[1].alt[0].seq, second);

    tree.undo().unwrap();
    let entries = tree.entries();
    assert!(entries[1].curhead, "the undone header is b_u_curhead");
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

/// Emits Edits written by ox-text, then asks real upstream Neovim to load the
/// undo file and dump the buffer content after each consecutive `:undo`.
/// Returns the per-step dump paths (in undo order) plus the dir.
fn forward_undo_dumps(dir: &Path, text: &[u8], undo_bytes: &[u8]) -> Vec<PathBuf> {
    let text_path = dir.join("fwd-text.txt");
    let undo_path = dir.join("fwd.un~");
    fs::write(&text_path, text).unwrap();
    fs::write(&undo_path, undo_bytes).unwrap();
    let mut args = vec![
        "--headless".into(), "-u".into(), "NONE".into(), "-n".into(),
        text_path.display().to_string(),
        "-c".into(), command_arg(format!("rundo {}", undo_path.display())),
    ];
    let mut dumps = Vec::new();
    for step in 0..4 {
        let out = dir.join(format!("fwd-step-{step}"));
        dumps.push(out.clone());
        args.push("-c".into());
        args.push("silent! undo".into());
        args.push("-c".into());
        args.push(command_arg(format!("silent! call writefile(getline(1, '$'), '{}')", out.display())));
    }
    args.push("-c".into());
    args.push("qa!".into());
    run_nvim(&args);
    dumps
}

#[test]
fn ox_text_writes_undo_history_real_nvim_undoes_forward() {
    if !Path::new(nvim()).exists() {
        return;
    }
    let dir = oracle_dir("fwd-linear");
    let mut buffer = Buffer::from_bytes(b"a\nb\n").unwrap();
    let mut tree = UndoTree::new();

    // Three sequential edits, each its own undo block: line 2 b->c, c->d,
    // then append "e". Without the syncs they would be one header, which is
    // what a scripted run of three mutations actually produces.
    tree.record(edit_at(2, &bytes(&["b"]), &bytes(&["c"])), 1710000001);
    buffer.replace_lines(2, 2, &bytes(&["c"])).unwrap();
    tree.sync();
    tree.record(edit_at(2, &bytes(&["c"]), &bytes(&["d"])), 1710000002);
    buffer.replace_lines(2, 2, &bytes(&["d"])).unwrap();
    tree.sync();
    tree.record(edit_at(3, &[], &bytes(&["e"])), 1710000003);
    buffer.append_lines(2, &bytes(&["e"])).unwrap();

    assert_eq!(buffer.to_bytes(), b"a\nd\ne\n");
    let undo = UndoFile::from_tree(&buffer, &tree);
    undo.verify_buffer(&buffer).unwrap();
    let mut undo_bytes = Vec::new();
    undo.write(&mut undo_bytes).unwrap();
    assert_eq!(written_header_count(&undo_bytes), 3);

    let dumps = forward_undo_dumps(&dir, b"a\nd\ne\n", &undo_bytes);
    assert_eq!(fs::read_to_string(&dumps[0]).unwrap(), "a\nd\n");
    assert_eq!(fs::read_to_string(&dumps[1]).unwrap(), "a\nc\n");
    assert_eq!(fs::read_to_string(&dumps[2]).unwrap(), "a\nb\n");
    // Already at oldest: content must not change.
    assert_eq!(fs::read_to_string(&dumps[3]).unwrap(), "a\nb\n");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn ox_text_writes_branching_history_real_nvim_follows_active_branch() {
    if !Path::new(nvim()).exists() {
        return;
    }
    let dir = oracle_dir("fwd-branch");
    let mut buffer = Buffer::from_bytes(b"a\nb\n").unwrap();
    let mut tree = UndoTree::new();

    // E1 b->c, E2 c->d, then abandon E2 (undo) and branch fresh edit E3, each
    // in its own undo block.
    tree.record(edit_at(2, &bytes(&["b"]), &bytes(&["c"])), 1710000001);
    buffer.replace_lines(2, 2, &bytes(&["c"])).unwrap();
    tree.sync();
    tree.record(edit_at(2, &bytes(&["c"]), &bytes(&["d"])), 1710000002);
    buffer.replace_lines(2, 2, &bytes(&["d"])).unwrap();
    buffer.replace_lines(2, 2, &bytes(&["c"])).unwrap(); // sync buffer to E1 state
    tree.undo().unwrap();
    tree.record(edit_at(2, &bytes(&["c"]), &bytes(&["e"])), 1710000003);
    buffer.replace_lines(2, 2, &bytes(&["e"])).unwrap();

    assert_eq!(buffer.to_bytes(), b"a\ne\n");
    let undo = UndoFile::from_tree(&buffer, &tree);
    undo.verify_buffer(&buffer).unwrap();
    let mut undo_bytes = Vec::new();
    undo.write(&mut undo_bytes).unwrap();
    // All three headers (including the abandoned E2 sibling) are serialized.
    assert_eq!(written_header_count(&undo_bytes), 3);

    let dumps = forward_undo_dumps(&dir, b"a\ne\n", &undo_bytes);
    // Active branch is E1 -> E3; undoing goes E3 then E1, never E2.
    assert_eq!(fs::read_to_string(&dumps[0]).unwrap(), "a\nc\n");
    assert_eq!(fs::read_to_string(&dumps[1]).unwrap(), "a\nb\n");
    assert_eq!(fs::read_to_string(&dumps[2]).unwrap(), "a\nb\n");
    assert_eq!(fs::read_to_string(&dumps[3]).unwrap(), "a\nb\n");
    fs::remove_dir_all(dir).unwrap();
}

/// A header holding several edits must read back as one undoable unit in real
/// Neovim: one `:undo` puts every line the block touched back at once.
///
/// This is the file-format half of the grouping change — a header carries an
/// entry list upstream (`uh_entry`, `undo.c:610-611`), and this proves our
/// writer emits that list in an order Neovim replays correctly.
#[test]
fn ox_text_writes_a_grouped_block_real_nvim_undoes_it_in_one_step() {
    if !Path::new(nvim()).exists() {
        return;
    }
    let dir = oracle_dir("fwd-grouped");
    let mut buffer = Buffer::from_bytes(b"a\nb\nc\n").unwrap();
    let mut tree = UndoTree::new();

    // One block whose edits are order-sensitive: delete line 1 twice, as
    // `:g/^[ab]$/d` does. Undoing has to reinsert "b" before "a", so a
    // wrongly ordered entry list produces "b\na\nc\n" instead.
    for gone in ["a", "b"] {
        tree.record(edit_at(1, &bytes(&[gone]), &[]), 1710000001);
        buffer.replace_lines(1, 1, &[]).unwrap();
    }
    assert_eq!(buffer.to_bytes(), b"c\n");

    let undo = UndoFile::from_tree(&buffer, &tree);
    undo.verify_buffer(&buffer).unwrap();
    let mut undo_bytes = Vec::new();
    undo.write(&mut undo_bytes).unwrap();
    assert_eq!(written_header_count(&undo_bytes), 1, "two edits, one header");

    let dumps = forward_undo_dumps(&dir, b"c\n", &undo_bytes);
    // The first undo restores both lines in their original order, and there
    // is nothing older.
    assert_eq!(fs::read_to_string(&dumps[0]).unwrap(), "a\nb\nc\n");
    assert_eq!(fs::read_to_string(&dumps[1]).unwrap(), "a\nb\nc\n");
    fs::remove_dir_all(dir).unwrap();
}

fn edit_at(start: usize, before: &[Vec<u8>], after: &[Vec<u8>]) -> LineEdit {
    LineEdit {
        start,
        before: before.to_vec(),
        after: after.to_vec(),
        cursor_before: Cursor { lnum: start, col: 0 },
        cursor_after: Cursor { lnum: start, col: 0 },
    }
}

/// Counts the leading header records (each begins with `0x5fd0`) in an
/// emitted persistent-undo byte stream.
fn written_header_count(undo_bytes: &[u8]) -> usize {
    let magic = [0x5f, 0xd0];
    undo_bytes
        .windows(2)
        .filter(|window| **window == magic)
        .count()
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

#[test]
fn undo_file_header_seq_validation() {
    let buffer = Buffer::from_bytes(b"a\nb\n").unwrap();
    let mut tree = UndoTree::new();
    tree.record(edit_at(2, &bytes(&["b"]), &bytes(&["c"])), 1710000001);
    tree.sync();
    tree.record(edit_at(2, &bytes(&["c"]), &bytes(&["d"])), 1710000002);
    let undo = UndoFile::from_tree(&buffer, &tree);
    let mut valid = Vec::new();
    undo.write(&mut valid).unwrap();
    assert!(UndoFile::read(IoCursor::new(valid.clone())).is_ok());

    let header_positions: Vec<usize> = valid
        .windows(2)
        .enumerate()
        .filter(|(_, w)| **w == [0x5f, 0xd0])
        .map(|(i, _)| i)
        .collect();
    assert_eq!(header_positions.len(), 2, "fixture must contain two headers");

    let seq_offset = |header_index: usize| header_positions[header_index] + 2 + 4 * 4;

    // A zero sequence number on the second header must be rejected.
    let mut zero_seq = valid.clone();
    zero_seq[seq_offset(1)..seq_offset(1) + 4].copy_from_slice(&0_i32.to_be_bytes());
    assert!(matches!(
        UndoFile::read(IoCursor::new(zero_seq)),
        Err(UndoFileError::Malformed)
    ));

    // A negative sequence number on the second header must be rejected.
    let mut neg_seq = valid.clone();
    neg_seq[seq_offset(1)..seq_offset(1) + 4].copy_from_slice(&i32::MIN.to_be_bytes());
    assert!(matches!(
        UndoFile::read(IoCursor::new(neg_seq)),
        Err(UndoFileError::Malformed)
    ));

    // A duplicate sequence number across headers must be rejected.
    let mut dup_seq = valid.clone();
    dup_seq[seq_offset(1)..seq_offset(1) + 4]
        .copy_from_slice(&valid[seq_offset(0)..seq_offset(0) + 4]);
    assert!(matches!(
        UndoFile::read(IoCursor::new(dup_seq)),
        Err(UndoFileError::Malformed)
    ));
}

#[test]
fn undo_file_entry_fields_reject_negative() {
    let buffer = Buffer::from_bytes(b"a\nb\n").unwrap();
    let mut tree = UndoTree::new();
    tree.record(edit_at(2, &bytes(&["b"]), &bytes(&["c"])), 1710000001);
    let undo = UndoFile::from_tree(&buffer, &tree);
    let mut valid = Vec::new();
    undo.write(&mut valid).unwrap();
    assert!(UndoFile::read(IoCursor::new(valid.clone())).is_ok());

    let entry_positions: Vec<usize> = valid
        .windows(2)
        .enumerate()
        .filter(|(_, w)| **w == [0xf5, 0x18])
        .map(|(i, _)| i)
        .collect();
    assert_eq!(entry_positions.len(), 1, "fixture must contain one entry");
    let entry_top = entry_positions[0] + 2;
    let neg = i32::MIN.to_be_bytes();

    for (name, field_offset) in [("ue_top", 0), ("ue_bot", 4), ("ue_lcount", 8)] {
        let mut mutated = valid.clone();
        let offset = entry_top + field_offset;
        mutated[offset..offset + 4].copy_from_slice(&neg);
        assert!(
            matches!(
                UndoFile::read(IoCursor::new(mutated)),
                Err(UndoFileError::Malformed)
            ),
            "negative {name} was accepted"
        );
    }
}

#[test]
fn replace_lines_disjoint_single_tick_and_geometry() {
    let mut buffer = Buffer::from_bytes(b"a\nb\nc\nd\n").unwrap();
    let tick = buffer.changedtick();
    let first = bytes(&["A", "AA"]);
    let third = bytes(&["C"]);
    buffer
        .replace_lines_disjoint(&[
            LineSplice {
                start: 1,
                end: 1,
                lines: &first,
            },
            LineSplice {
                start: 3,
                end: 3,
                lines: &third,
            },
        ])
        .unwrap();
    assert_eq!(buffer.changedtick(), tick + 1);
    assert_eq!(buffer.to_bytes(), b"A\nAA\nb\nC\nd\n");
}

#[test]
fn replace_lines_disjoint_validates_before_mutating() {
    let mut buffer = Buffer::from_bytes(b"a\nb\nc\n").unwrap();
    let tick = buffer.changedtick();
    let original = buffer.to_bytes();
    let bad = vec![b"x\ny".to_vec()];
    assert_eq!(
        buffer.replace_lines_disjoint(&[LineSplice {
            start: 1,
            end: 1,
            lines: &bad,
        }]),
        Err(BufferError::NewlineInLine)
    );
    assert_eq!(buffer.to_bytes(), original);
    assert_eq!(buffer.changedtick(), tick);

    assert!(matches!(
        buffer.replace_lines_disjoint(&[LineSplice {
            start: 8,
            end: 8,
            lines: &bytes(&["z"]),
        }]),
        Err(BufferError::LineRange { .. })
    ));
    assert_eq!(buffer.to_bytes(), original);
    assert_eq!(buffer.changedtick(), tick);

    let one = bytes(&["A"]);
    let two = bytes(&["B"]);
    assert_eq!(
        buffer.replace_lines_disjoint(&[
            LineSplice {
                start: 1,
                end: 2,
                lines: &one,
            },
            LineSplice {
                start: 2,
                end: 3,
                lines: &two,
            },
        ]),
        Err(BufferError::OverlappingSplices)
    );
    assert_eq!(buffer.to_bytes(), original);
    assert_eq!(buffer.changedtick(), tick);
}

#[test]
fn replace_lines_disjoint_empty_is_noop() {
    let mut buffer = Buffer::from_bytes(b"a\nb\n").unwrap();
    let tick = buffer.changedtick();
    let original = buffer.to_bytes();
    buffer.replace_lines_disjoint(&[]).unwrap();
    assert_eq!(buffer.changedtick(), tick);
    assert_eq!(buffer.to_bytes(), original);
}
