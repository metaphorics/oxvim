//! Filesystem-backed Vimscript builtins routed exclusively through [`FileIO`].
//!
//! Semantics follow Neovim `src/nvim/eval/fs.c`: `f_delete` (438-470),
//! metadata functions (527-539, 834-887), `f_glob`/`f_globpath` (924-1014),
//! `f_swapfilelist` (7200) via `recover_names` (memline.c 1303-1429),
//! `f_mkdir` (1087-1157), `read_file_or_blob` (1299-1496), `f_rename`
//! (1512-1521), and `f_writefile`/`write_list` (1714-1760, 1802-1906).

use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use ox_eval::{builtin_spec, EvalError};
use ox_types::{OxStr, Typval};

use crate::script::{FileIO, FileKind};

pub(crate) fn is_filesystem_builtin(name: &str) -> bool {
    matches!(
        name,
        "mkdir" | "delete" | "rename" | "filecopy" | "readblob" | "glob" | "globpath"
            | "readfile" | "writefile" | "filereadable" | "isdirectory" | "getftime"
            | "getfsize" | "getfperm" | "filewritable" | "setfperm"
    )
}

/// Routes one filesystem builtin that needs nothing but the [`FileIO`] seam.
///
/// `mkdir` and `writefile` are not here: their `D`/`R` flags register a
/// deferred delete against the enclosing function frame, so they are called
/// from [`crate::builtins::filesystem`] with that context.
pub(crate) fn call(io: &dyn FileIO, name: &str, args: Vec<Typval>) -> ox_eval::Result<Typval> {
    check_arity(name, args.len())?;
    match name {
        "delete" => delete(io, &args),
        "rename" => rename(io, &args),
        "filecopy" => filecopy(io, &args),
        "readblob" => readblob(io, &args),
        "glob" => glob(io, &args),
        "globpath" => globpath(io, &args),
        "readfile" => readfile(io, &args),
        "filereadable" => filereadable(io, &args[0]),
        "isdirectory" => isdirectory(io, &args[0]),
        "getftime" => getftime(io, &args[0]),
        "getfsize" => getfsize(io, &args[0]),
        "getfperm" => getfperm(io, &args[0]),
        "filewritable" => filewritable(io, &args[0]),
        "setfperm" => setfperm(io, &args),
        _ => unreachable!("filesystem builtin predicate and dispatcher disagree: {name}"),
    }
}

pub(crate) fn check_writefile_arity(count: usize) -> ox_eval::Result<()> {
    check_arity("writefile", count)
}

pub(crate) fn check_arity(name: &str, count: usize) -> ox_eval::Result<()> {
    let spec = builtin_spec(name).ok_or_else(|| EvalError::not_implemented(OxStr::from(name)))?;
    if count < spec.min_args {
        return Err(EvalError::new("E119", 0, format!("Not enough arguments for function: {name}")));
    }
    if spec.max_args.is_some_and(|maximum| count > maximum) {
        return Err(EvalError::new("E118", 0, format!("Too many arguments for function: {name}")));
    }
    Ok(())
}

/// `f_mkdir` (`eval/fs.c:1087-1157`).
///
/// `can_defer` is `can_add_defer()` (`eval/userfunc.c` 3457-3464), checked
/// before anything is created when the flags ask for a deferred delete.
/// `deferred` reports the directory whose deletion a `D`/`R` flag registered
/// against the enclosing function frame: the *first* directory the recursive
/// walk created (`os/fs.c:1079-1081`), or the full name when nothing was
/// created (1143-1146).
pub(crate) fn mkdir(
    io: &dyn FileIO,
    args: &[Typval],
    can_defer: bool,
    deferred: &mut Option<(PathBuf, DeleteMode)>,
) -> ox_eval::Result<Typval> {
    let mut name = string_arg(&args[0])?;
    if name.is_empty() {
        // FAIL, reported through the return value only (1099-1101).
        return Ok(number(0));
    }
    // Remove trailing slashes (1103-1106).
    while name.ends_with('/') {
        name.pop();
    }
    let mut prot: i64 = 0o755;
    let mut defer = false;
    let mut defer_recurse = false;
    let mut created: Option<PathBuf> = None;
    let mut outcome: i64 = 0;
    if args.len() > 1 {
        if args.len() > 2 {
            // `prot` is read before the flags string (1112-1117), so
            // `mkdir('abc', [], [])` reports E745 and not E730.
            prot = number_arg(&args[2])?;
            if prot == -1 {
                return Ok(number(0));
            }
        }
        let flags = string_arg(&args[1])?;
        defer = flags.contains('D');
        defer_recurse = flags.contains('R');
        if (defer || defer_recurse) && !can_defer {
            // `can_add_defer`'s own message, emitted before anything is created.
            return Err(EvalError::new("E193", 0, "defer not inside a function"));
        }
        if flags.contains('p') {
            match mkdir_recurse(io, Path::new(&name), prot as u32) {
                Ok(first) => {
                    created = first;
                    outcome = 1;
                }
                Err((failed, error)) => return Err(e739(&failed, &error)),
            }
        }
    }
    if outcome == 0 {
        // `vim_mkdir_emsg` (ex_docmd.c:7006-7015).
        if let Err(error) = io.create_dir(Path::new(&name), false, prot as u32) {
            return Err(e739(Path::new(&name), &error));
        }
        outcome = 1;
    }
    if defer || defer_recurse {
        if created.is_none() {
            // Nothing was created — the deferred delete targets the directory
            // itself (1143-1146).
            created = Some(io.canonicalize(Path::new(&name)));
        }
        if let Some(created) = created {
            // `add_defer("delete", ...)` with "rf" for 'R' and "d" otherwise
            // (1147-1156).
            *deferred = Some((
                created,
                if defer_recurse { DeleteMode::Recursive } else { DeleteMode::Dir },
            ));
        }
    }
    Ok(number(outcome))
}

/// `os_mkdir_recurse` (`os/fs.c:1042-1085`): walk up to the deepest existing
/// ancestor, then create every missing component. Returns the full name of
/// the first directory this call created (1079-1081), or `None` when every
/// component already existed; the failing component is reported for `E739`.
fn mkdir_recurse(
    io: &dyn FileIO,
    dir: &Path,
    mode: u32,
) -> Result<Option<PathBuf>, (PathBuf, std::io::Error)> {
    let mut missing: Vec<PathBuf> = Vec::new();
    let mut current = dir.to_path_buf();
    loop {
        if io.metadata(&current, true).is_ok_and(|metadata| metadata.kind == FileKind::Directory) {
            break;
        }
        missing.push(current.clone());
        match current.parent() {
            // An empty parent is the head of a relative path, the base the
            // walk creates against (`get_past_head`).
            Some(parent) if !parent.as_os_str().is_empty() => current = parent.to_path_buf(),
            _ => break,
        }
    }
    let mut created = None;
    for dir in missing.into_iter().rev() {
        io.create_dir(&dir, false, mode).map_err(|error| (dir.clone(), error))?;
        if created.is_none() {
            created = Some(io.canonicalize(&dir));
        }
    }
    Ok(created)
}

/// `e_mkdir` (`errors.h:55`): "Cannot create directory %s: %s".
fn e739(path: &Path, error: &std::io::Error) -> EvalError {
    EvalError::new("E739", 0, format!("Cannot create directory {}: {}", path.display(), error))
}

/// The three `delete()` flag strings (`eval/fs.c:459-470`), shared by the
/// builtin and by every deferred delete `writefile(..., 'D')`,
/// `mkdir(..., 'D'/'R')`, and `:defer delete()` register on a function
/// frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeleteMode {
    /// `""` — remove one file.
    File,
    /// `"d"` — remove an empty directory.
    Dir,
    /// `"rf"` — remove a directory tree.
    Recursive,
}

impl DeleteMode {
    /// Parse the three `delete()` flag strings (`eval/fs.c:459-470`):
    /// `""` removes one file, `"d"` removes an empty directory, and
    /// `"rf"` removes a directory tree. Anything else raises `E15`.
    pub(crate) fn parse(flags: &str) -> ox_eval::Result<Self> {
        match flags {
            "" => Ok(DeleteMode::File),
            "d" => Ok(DeleteMode::Dir),
            "rf" => Ok(DeleteMode::Recursive),
            _ => Err(EvalError::new("E15", 0, format!("Invalid expression: {flags}"))),
        }
    }
    /// The `os_remove`/`os_rmdir`/`delete_recursive` dispatch behind each
    /// flag (461-467).
    pub(crate) fn remove(self, io: &dyn FileIO, path: &Path) -> std::io::Result<()> {
        match self {
            DeleteMode::File => io.remove_file(path),
            DeleteMode::Dir => io.remove_dir(path),
            DeleteMode::Recursive => io.remove_dir_all(path),
        }
    }
}

fn delete(io: &dyn FileIO, args: &[Typval]) -> ox_eval::Result<Typval> {
    let path = path_arg(&args[0])?;
    if path.as_os_str().is_empty() {
        return Err(EvalError::new("E474", 0, "Invalid argument"));
    }
    let flags = optional_string(args.get(1))?.unwrap_or_default();
    let mode = DeleteMode::parse(&flags)?;
    Ok(number(if mode.remove(io, &path).is_ok() { 0 } else { -1 }))
}
/// `f_rename` (`eval/fs.c:1512-1521`) delegates to `vim_rename`
/// (fileio.c:2710-2766): a normal rename, falling back to copy-then-unlink
/// when the OS rename fails (cross-device, EXDEV).  The unlink after a
/// successful copy is best-effort — a read-only source cannot be removed,
/// but the copy has already succeeded, so the operation is treated as
/// success.  This lets `Test_rename_copy` restore `Xrenamedir` permissions
/// after the copy fallback, so subsequent `test_retab` cleanup can remove
/// the fixture normally.
fn rename(io: &dyn FileIO, args: &[Typval]) -> ox_eval::Result<Typval> {
    let from = path_arg(&args[0])?;
    let to = path_arg(&args[1])?;
    if from.components().eq(to.components()) {
        return Ok(number(0));
    }
    if io.rename(&from, &to).is_ok() {
        return Ok(number(0));
    }
    // Rename failed — try copy-then-unlink, matching `vim_rename`'s fallback.
    if io.copy_file(&from, &to).is_err() {
        return Ok(number(-1));
    }
    // Best-effort unlink: a read-only source cannot be removed, but the copy
    // succeeded, so treat the operation as success (vim_rename:2761-2763).
    let _ = io.remove_file(&from);
    Ok(number(0))
}

/// `f_filecopy` (`eval/fs.c:505-524`): copies a regular file or symbolic
/// link.  Returns `1` on success, `0` on failure or when the source is
/// neither a regular file nor a symlink.
fn filecopy(io: &dyn FileIO, args: &[Typval]) -> ox_eval::Result<Typval> {
    let from = strict_path_arg(&args[0], 1)?;
    let to = strict_path_arg(&args[1], 2)?;
    let eligible = io.metadata(&from, false).is_ok_and(|metadata| {
        matches!(metadata.kind, FileKind::File | FileKind::Symlink)
    });
    if !eligible {
        return Ok(boolean(false));
    }
    Ok(boolean(io.copy_file(&from, &to).is_ok()))
}

/// `f_readblob` (`eval/fs.c:1492-1500`) reads a file as a binary blob.
/// An optional offset (default 0) and size (default -1, whole file) limit
/// the read.  A negative offset counts from the end of the file.  Only
/// `size == -1` means whole file; any other non-positive size returns an
/// empty blob, matching `read_blob`'s `size <= 0` early return
/// (fs.c:1277-1279).  A read failure reports `E485`
/// (`e_cant_read_file_str`, fs.c:1352).
fn readblob(io: &dyn FileIO, args: &[Typval]) -> ox_eval::Result<Typval> {
    let path = path_arg(&args[0])?;
    let offset = args.get(1).map(number_arg).transpose()?.unwrap_or(0);
    let size = args.get(2).map(number_arg).transpose()?.unwrap_or(-1);
    // `size == -1` reads through end of file; any other `size <= 0` (for
    // example -2) returns an empty blob, matching `read_blob`'s early return.
    if size <= 0 && size != -1 {
        return Ok(Typval::Blob(Vec::new()));
    }
    let bytes = match io.read_bytes_range(&path, offset, size) {
        Ok(bytes) => bytes,
        Err(error) => {
            return Err(match error.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied => {
                    EvalError::new("E484", 0, format!("Can't open file {}", path.display()))
                }
                _ => EvalError::new("E485", 0, format!("Can't read file {}", path.display())),
            });
        }
    };
    Ok(Typval::Blob(bytes))
}

fn filereadable(io: &dyn FileIO, value: &Typval) -> ox_eval::Result<Typval> {
    let path = path_arg(value)?;
    let readable = io.metadata(&path, true).is_ok_and(|metadata| {
        metadata.kind == FileKind::File && metadata.mode & 0o444 != 0
    });
    Ok(boolean(readable))
}

fn isdirectory(io: &dyn FileIO, value: &Typval) -> ox_eval::Result<Typval> {
    let path = path_arg(value)?;
    Ok(boolean(io.metadata(&path, true).is_ok_and(|metadata| metadata.kind == FileKind::Directory)))
}

fn getftime(io: &dyn FileIO, value: &Typval) -> ox_eval::Result<Typval> {
    let path = path_arg(value)?;
    let seconds = io.metadata(&path, true).ok()
        .and_then(|metadata| metadata.modified)
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or(-1);
    Ok(number(seconds))
}

fn getfsize(io: &dyn FileIO, value: &Typval) -> ox_eval::Result<Typval> {
    let path = path_arg(value)?;
    let size = match io.metadata(&path, true) {
        Ok(metadata) if metadata.kind == FileKind::Directory => 0,
        Ok(metadata) => i64::try_from(metadata.len).unwrap_or(-2),
        Err(_) => -1,
    };
    Ok(number(size))
}

fn getfperm(io: &dyn FileIO, value: &Typval) -> ox_eval::Result<Typval> {
    let path = path_arg(value)?;
    let Some(mode) = io.metadata(&path, true).ok().map(|metadata| metadata.mode) else {
        return Ok(text(""));
    };
    let flags = [b'r', b'w', b'x'];
    let mut output = [b'-'; 9];
    for (index, slot) in output.iter_mut().enumerate() {
        if mode & (1 << (8 - index)) != 0 {
            *slot = flags[index % 3];
        }
    }
    Ok(Typval::String(OxStr(output.to_vec())))
}

fn filewritable(io: &dyn FileIO, value: &Typval) -> ox_eval::Result<Typval> {
    let path = path_arg(value)?;
    let writable = io.metadata(&path, true).map_or(0, |metadata| {
        if metadata.mode & 0o222 == 0 { 0 }
        else if metadata.kind == FileKind::Directory { 2 }
        else if metadata.kind == FileKind::File { 1 }
        else { 0 }
    });
    Ok(number(writable))
}

fn setfperm(io: &dyn FileIO, args: &[Typval]) -> ox_eval::Result<Typval> {
    let path = path_arg(&args[0])?;
    let permission = string_arg(&args[1])?;
    if permission.len() != 9 { return Ok(number(0)); }
    let expected = [b'r', b'w', b'x'];
    let mut mode = 0u32;
    for (index, byte) in permission.bytes().enumerate() {
        if byte == expected[index % 3] { mode |= 1 << (8 - index); }
        else if byte != b'-' { return Ok(number(0)); }
    }
    Ok(boolean(io.set_permissions(&path, mode).is_ok()))
}

/// `read_file_or_blob` (`eval/fs.c:1299-1496`) in its list form. The type
/// argument is an exact match (1320-1324): `"b"` reads binary, `"B"` returns
/// the whole file as a Blob, and any other string — including `"bb"` — means
/// text.
fn readfile(io: &dyn FileIO, args: &[Typval]) -> ox_eval::Result<Typval> {
    let path = path_arg(&args[0])?;
    let kind = optional_string(args.get(1))?.unwrap_or_default();
    let binary = kind == "b";
    let blob = kind == "B";
    let maximum = args.get(2).map(number_arg).transpose()?;
    let bytes = io.read_bytes(&path)
        .map_err(|_| EvalError::new("E484", 0, format!("Can't open file {}", path.display())))?;
    if blob {
        return Ok(Typval::Blob(bytes));
    }
    // The upstream scan (1370-1460) makes one pass that converts each NUL it
    // walks over — after the line-splitting test for that byte, so a NUL
    // never splits a line — and, in text mode, drops every EF BB BF triple
    // it finds. A BOM cannot span a line boundary (the '\n' would break the
    // EF BB BF adjacency), so filtering the whole buffer first is equivalent.
    let content = if binary { bytes } else { remove_byte_order_marks(&bytes) };
    let ended_in_newline = content.last() == Some(&b'\n');
    let mut lines: Vec<Vec<u8>> = content.split(|byte| *byte == b'\n').map(|line| {
        let mut line = line.to_vec();
        if !binary {
            // Remove CRs before NL (1378-1388) — all of them.
            while line.last() == Some(&b'\r') { line.pop(); }
        }
        for byte in &mut line { if *byte == 0 { *byte = b'\n'; } }
        line
    }).collect();
    if content.is_empty() {
        // Text mode flushes nothing for an empty file; binary mode's final
        // pass still appends one empty line (1371), so [''] survives.
        if !binary { lines.clear(); }
    } else if !binary && ended_in_newline {
        lines.pop();
    }
    if let Some(maximum) = maximum {
        if maximum >= 0 {
            lines.truncate(maximum as usize);
        } else {
            let retain = maximum.unsigned_abs() as usize;
            if lines.len() > retain { lines.drain(..lines.len() - retain); }
        }
    }
    Ok(Typval::list(lines.into_iter().map(|line| Typval::String(OxStr(line))).collect()))
}

/// Text mode removes EF BB BF at any position (1426-1459), not only the one
/// at byte 0.
fn remove_byte_order_marks(bytes: &[u8]) -> Vec<u8> {
    if !bytes.windows(3).any(|window| window == [0xef, 0xbb, 0xbf]) {
        return bytes.to_vec();
    }
    let mut cleaned = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index..].starts_with(&[0xef, 0xbb, 0xbf]) {
            index += 3;
        } else {
            cleaned.push(bytes[index]);
            index += 1;
        }
    }
    cleaned
}

/// Flags `f_writefile` accepts (`eval/fs.c` 1835-1860).
///
/// `s`/`S` force and suppress the `fsync` that `file_close(&fp, do_fsync)`
/// performs (1902). This port writes through [`FileIO`], which has no
/// durability control to force or suppress, so both letters are accepted and
/// change nothing observable — the bytes are already in the file either way.
/// Rejecting them would be the only observable difference, and it is the wrong
/// one.
struct WriteFlags {
    binary: bool,
    append: bool,
    defer: bool,
    mkdir: bool,
}

fn write_flags(flags: &str) -> ox_eval::Result<WriteFlags> {
    let mut parsed = WriteFlags { binary: false, append: false, defer: false, mkdir: false };
    for (offset, flag) in flags.char_indices() {
        match flag {
            'b' => parsed.binary = true,
            'a' => parsed.append = true,
            'D' => parsed.defer = true,
            's' | 'S' => {}
            'p' => parsed.mkdir = true,
            // `semsg(_("E5060: Unknown flag: %s"), p)` prints the rest of the
            // string from the offending byte, not just that one character, so
            // a multibyte flag survives the message intact.
            _ => return Err(EvalError::new("E5060", 0, format!("Unknown flag: {}", &flags[offset..]))),
        }
    }
    Ok(parsed)
}

/// `f_writefile` (`eval/fs.c` 1802-1907).
///
/// `deferred` reports the absolute path a `D` flag asked to have deleted when
/// the enclosing function returns; `in_function` is `can_add_defer()`
/// (`eval/userfunc.c` 3457-3464), which is checked before the file is opened.
pub(crate) fn writefile(
    io: &dyn FileIO,
    args: &[Typval],
    in_function: bool,
    deferred: &mut Option<PathBuf>,
) -> ox_eval::Result<Typval> {
    // Upstream reads the flags (1835) before the file name (1863).
    let flags = write_flags(&optional_string(args.get(2))?.unwrap_or_default())?;
    let path = path_arg(&args[1])?;
    if path.as_os_str().is_empty() {
        return Err(EvalError::new("E482", 0, "Can't open file with an empty name"));
    }
    // `can_add_defer` runs before `file_open` (1868-1870), so a `D` outside a
    // function leaves no file behind.
    if flags.defer && !in_function {
        return Err(EvalError::new("E193", 0, "defer not inside a function"));
    }
    // `kFileMkDir` creates the parent chain as part of the open, so a `p`
    // failure is reported as the open failure it is.
    if flags.mkdir {
        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            io.create_dir(parent, true, 0o755).map_err(|error| {
                EvalError::new("E482", 0, format!("Can't open file {path:?} for writing: {error}"))
            })?;
        }
    }
    let bytes = write_data(&args[0], flags.binary)?;
    io.write_bytes(&path, &bytes, flags.append)
        .map_err(|error| EvalError::new("E482", 0, format!("Can't open file {path:?} for writing: {error}")))?;
    if flags.defer {
        // `add_defer("delete", 1, &tv)` with `FullName_save(fname, false)`
        // (1882-1889): the path is absolutized now, so a later `:cd` cannot
        // move the deletion target.
        *deferred = Some(io.canonicalize(&path));
    }
    Ok(number(0))
}

fn write_data(value: &Typval, binary: bool) -> ox_eval::Result<Vec<u8>> {
    match value {
        Typval::Blob(bytes) => Ok(bytes.clone()),
        Typval::String(value) => Ok(value.as_bytes().to_vec()),
        Typval::List(list) => {
            let list = list.try_borrow().map_err(|_| EvalError::new("E742", 0, "List is locked"))?;
            let mut output = Vec::new();
            for (index, item) in list.items.iter().enumerate() {
                let mut line = value_bytes(item)?;
                for byte in &mut line { if *byte == b'\n' { *byte = 0; } }
                output.extend_from_slice(&line);
                if !binary || index + 1 < list.items.len() { output.push(b'\n'); }
            }
            Ok(output)
        }
        _ => Err(EvalError::new("E474", 0, "writefile() first argument must be a List, String, or Blob")),
    }
}

fn glob(io: &dyn FileIO, args: &[Typval]) -> ox_eval::Result<Typval> {
    let pattern = string_arg(&args[0])?;
    let list = args.get(2).map(bool_arg).transpose()?.unwrap_or(false);
    let all_links = args.get(3).map(bool_arg).transpose()?.unwrap_or(false);
    glob_result(expand_glob(io, &pattern, all_links), list)
}

fn globpath(io: &dyn FileIO, args: &[Typval]) -> ox_eval::Result<Typval> {
    let paths = string_arg(&args[0])?;
    let pattern = string_arg(&args[1])?;
    let list = args.get(3).map(bool_arg).transpose()?.unwrap_or(false);
    let all_links = args.get(4).map(bool_arg).transpose()?.unwrap_or(false);
    let mut matches = Vec::new();
    for directory in split_path_list(&paths) {
        matches.extend(expand_glob(io, &Path::new(&directory).join(&pattern).to_string_lossy(), all_links));
    }
    matches.sort();
    matches.dedup();
    glob_result(matches, list)
}

fn glob_result(matches: Vec<String>, list: bool) -> ox_eval::Result<Typval> {
    if list {
        Ok(Typval::list(matches.into_iter().map(text).collect()))
    } else {
        Ok(text(matches.join("\n")))
    }
}

/// `swapfilelist()` — upstream `f_swapfilelist` (eval/funcs.c 7200) delegates
/// to `recover_names(NULL, false, list)` (memline.c 1303): for every directory
/// in the 'directory' option, expand `*.sw?`, `.*.sw?`, and `.sw?` and collect
/// the matches. Each pattern's matches are appended in pattern order with
/// duplicates kept across patterns — `EW_KEEPALL` only skips 'wildignore' and
/// 'suffixes' filtering (path.c 2129-2141); there is no cross-pattern dedup.
pub(crate) fn swapfilelist(io: &dyn FileIO, arg_count: usize, directory: &str) -> ox_eval::Result<Typval> {
    check_arity("swapfilelist", arg_count)?;
    let mut matches = Vec::new();
    for dir in split_path_list(directory) {
        if dir.is_empty() {
            continue;
        }
        let patterns: Vec<String> = if dir == "." {
            ["*.sw?", ".*.sw?", ".sw?"].map(str::to_owned).into()
        } else {
            // Upstream concat_fnames(dir, pattern, true) joins with one
            // separator (memline.c 1350-1354).
            let base = PathBuf::from(&dir);
            ["*.sw?", ".*.sw?", ".sw?"]
                .map(|pattern| base.join(pattern).to_string_lossy().into_owned())
                .into()
        };
        for pattern in patterns {
            matches.extend(expand_glob(io, &pattern, false).into_iter().map(|name| {
                // Upstream expands the bare relative patterns of the "."
                // branch, so names carry no "./" prefix (memline.c 1339-1343);
                // our globber anchors at the current directory instead.
                name.strip_prefix("./").map(str::to_owned).unwrap_or(name)
            }));
        }
    }
    Ok(Typval::list(matches.into_iter().map(text).collect()))
}

pub(crate) fn expand_glob(io: &dyn FileIO, pattern: &str, all_links: bool) -> Vec<String> {
    let expanded;
    let pattern = if (pattern == "~" || pattern.starts_with("~/")) && std::env::var_os("HOME").is_some() {
        expanded = format!("{}{}", PathBuf::from(std::env::var_os("HOME").expect("checked above")).to_string_lossy(), &pattern[1..]);
        expanded.as_str()
    } else {
        pattern
    };
    let path = Path::new(pattern);
    let absolute = path.is_absolute();
    let components: Vec<String> = path.components().filter_map(|component| match component {
        Component::RootDir => None,
        Component::CurDir => Some(".".to_owned()),
        Component::ParentDir => Some("..".to_owned()),
        Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
        Component::Prefix(value) => Some(value.as_os_str().to_string_lossy().into_owned()),
    }).collect();
    let base = if absolute { PathBuf::from("/") } else { PathBuf::new() };
    let mut output = Vec::new();
    expand_components(io, &base, &components, 0, all_links, &mut output);
    output.sort();
    output.dedup();
    output
}

fn expand_components(io: &dyn FileIO, base: &Path, components: &[String], index: usize, all_links: bool, output: &mut Vec<String>) {
    if index == components.len() {
        if io.metadata(base, !all_links).is_ok() {
            output.push(if base.as_os_str().is_empty() { ".".to_owned() } else { base.to_string_lossy().into_owned() });
        }
        return;
    }
    let component = &components[index];
    if component == "**" {
        expand_components(io, base, components, index + 1, all_links, output);
        let directory = if base.as_os_str().is_empty() { Path::new(".") } else { base };
        let mut entries = io.read_dir(directory).unwrap_or_default();
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        for entry in entries {
            let ordinary_directory = io.metadata(&entry.path, false).is_ok_and(|metadata| metadata.kind == FileKind::Directory);
            if ordinary_directory { expand_components(io, &entry.path, components, index, all_links, output); }
        }
        return;
    }
    if !has_wildcard(component) {
        expand_components(io, &base.join(component), components, index + 1, all_links, output);
        return;
    }
    let directory = if base.as_os_str().is_empty() { Path::new(".") } else { base };
    let mut entries = io.read_dir(directory).unwrap_or_default();
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    for entry in entries {
        let filename = entry.name.to_string_lossy();
        if filename.starts_with('.') && !component.starts_with('.') { continue; }
        if wildcard_match(component.as_bytes(), filename.as_bytes()) {
            expand_components(io, &entry.path, components, index + 1, all_links, output);
        }
    }
}

fn has_wildcard(component: &str) -> bool {
    component.bytes().any(|byte| matches!(byte, b'*' | b'?' | b'['))
}

/// Matches one file-name wildcard pattern (`*`, `?`, `[...]`) against a name.
pub(crate) fn wildcard_match(pattern: &[u8], value: &[u8]) -> bool {
    fn matches_at(pattern: &[u8], value: &[u8], pi: usize, vi: usize) -> bool {
        if pi == pattern.len() { return vi == value.len(); }
        match pattern[pi] {
            b'*' => (vi..=value.len()).any(|next| matches_at(pattern, value, pi + 1, next)),
            b'?' => vi < value.len() && matches_at(pattern, value, pi + 1, vi + 1),
            b'[' => {
                let Some(relative) = pattern.get(pi + 1..).and_then(|tail| tail.iter().position(|byte| *byte == b']')) else {
                    return vi < value.len() && value[vi] == b'[' && matches_at(pattern, value, pi + 1, vi + 1);
                };
                let close = pi + 1 + relative;
                let class = &pattern[pi + 1..close];
                let negated = class.first().is_some_and(|byte| matches!(byte, b'!' | b'^'));
                let class = if negated { &class[1..] } else { class };
                let mut accepted = false;
                let mut cursor = 0;
                while cursor < class.len() {
                    if cursor + 2 < class.len() && class[cursor + 1] == b'-' {
                        accepted |= vi < value.len() && (class[cursor]..=class[cursor + 2]).contains(&value[vi]);
                        cursor += 3;
                    } else {
                        accepted |= vi < value.len() && class[cursor] == value[vi];
                        cursor += 1;
                    }
                }
                vi < value.len() && accepted != negated && matches_at(pattern, value, close + 1, vi + 1)
            }
            literal => vi < value.len() && literal == value[vi] && matches_at(pattern, value, pi + 1, vi + 1),
        }
    }
    matches_at(pattern, value, 0, 0)
}

fn split_path_list(paths: &str) -> Vec<String> {
    let mut output = vec![String::new()];
    let mut escaped = false;
    for character in paths.chars() {
        if escaped { output.last_mut().expect("one path").push(character); escaped = false; }
        else if character == '\\' { escaped = true; }
        else if character == ',' { output.push(String::new()); }
        else { output.last_mut().expect("one path").push(character); }
    }
    if escaped { output.last_mut().expect("one path").push('\\'); }
    output
}

fn optional_string(value: Option<&Typval>) -> ox_eval::Result<Option<String>> {
    value.map(string_arg).transpose()
}

fn path_arg(value: &Typval) -> ox_eval::Result<PathBuf> {
    string_arg(value).map(PathBuf::from)
}

fn strict_path_arg(value: &Typval, argument: usize) -> ox_eval::Result<PathBuf> {
    match value {
        Typval::String(value) => Ok(PathBuf::from(value.to_string_lossy().into_owned())),
        _ => Err(EvalError::new("E1174", 0, format!("String required for argument {argument}"))),
    }
}

fn string_arg(value: &Typval) -> ox_eval::Result<String> {
    match value {
        Typval::String(value) => Ok(value.to_string_lossy().into_owned()),
        Typval::Number(value) => Ok(value.to_string()),
        _ => Err(EvalError::new("E730", 0, "Using a non-String as a String")),
    }
}

fn value_bytes(value: &Typval) -> ox_eval::Result<Vec<u8>> {
    match value {
        Typval::String(value) => Ok(value.as_bytes().to_vec()),
        Typval::Number(value) => Ok(value.to_string().into_bytes()),
        _ => Err(EvalError::new("E745", 0, "Using a non-String as a String")),
    }
}
fn number_arg(value: &Typval) -> ox_eval::Result<i64> {
    match value {
        Typval::Number(value) => Ok(*value),
        Typval::Bool(value) => Ok(i64::from(*value)),
        _ => Err(EvalError::new("E745", 0, "Using a non-Number as a Number")),
    }
}

fn bool_arg(value: &Typval) -> ox_eval::Result<bool> {
    number_arg(value).map(|value| value != 0)
}

fn number(value: i64) -> Typval { Typval::Number(value) }
fn boolean(value: bool) -> Typval { number(i64::from(value)) }
fn text(value: impl AsRef<str>) -> Typval { Typval::String(OxStr::from(value.as_ref())) }

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::{Editor, ExExecutor};
    use ox_eval::ScopeKind;
    use crate::script::RealFileIO;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let path = std::env::temp_dir().join(format!("ox-editor-{label}-{}-{nonce}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }

    fn path(path: &Path) -> Typval { text(path.to_string_lossy()) }

    /// `writefile` at top-level script scope: no function frame, so `D` is
    /// `E193` here and the deferred path is discarded.
    fn write(args: Vec<Typval>) -> ox_eval::Result<Typval> {
        check_writefile_arity(args.len())?;
        writefile(&RealFileIO, &args, false, &mut None)
    }

    #[test]
    fn executor_routes_filesystem_builtins_through_fileio() {
        let root = TempRoot::new("executor");
        let directory = root.0.join("made");
        let mut editor = Editor::new();
        let mut executor = ExExecutor::new();
        executor.execute_script(
            &mut editor,
            "<filesystem-test>",
            &format!("call mkdir('{}')\nlet g:isdir = isdirectory('{}')", directory.display(), directory.display()),
        ).unwrap();
        assert!(directory.is_dir());
        assert_eq!(executor.scope().global.iter().find(|(name, _)| name.as_bytes() == b"isdir").map(|(_, value)| value.clone()), Some(number(1)));
    }

    #[test]
    fn metadata_builtins_return_upstream_sentinels_and_permissions() {
        let root = TempRoot::new("metadata");
        let file = root.0.join("file");
        fs::write(&file, b"data").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
        }
        assert_eq!(call(&RealFileIO, "filereadable", vec![path(&file)]).unwrap(), number(1));
        assert_eq!(call(&RealFileIO, "isdirectory", vec![path(&root.0)]).unwrap(), number(1));
        assert_eq!(call(&RealFileIO, "getfsize", vec![path(&file)]).unwrap(), number(4));
        assert_eq!(call(&RealFileIO, "getfsize", vec![path(&root.0)]).unwrap(), number(0));
        assert_eq!(call(&RealFileIO, "getfsize", vec![path(&root.0.join("missing"))]).unwrap(), number(-1));
        assert!(matches!(call(&RealFileIO, "getftime", vec![path(&file)]).unwrap(), Typval::Number(value) if value > 0));
        #[cfg(unix)]
        assert_eq!(call(&RealFileIO, "getfperm", vec![path(&file)]).unwrap(), text("rw-r-----"));
        assert_eq!(call(&RealFileIO, "filewritable", vec![path(&root.0)]).unwrap(), number(2));
        assert_eq!(call(&RealFileIO, "filewritable", vec![path(&root.0.join("missing"))]).unwrap(), number(0));
    }

    #[test]
    fn mutation_builtins_create_rename_and_delete_exact_targets() {
        let root = TempRoot::new("mutation");
        let nested = root.0.join("one/two");
        // `f_mkdir` reports OK as 1, and no longer routes through `call`:
        // its `D`/`R` flags need the function-frame context.
        assert_eq!(mkdir(&RealFileIO, &[path(&nested), text("p"), number(0o700)], false, &mut None).unwrap(), number(1));
        assert!(nested.is_dir());
        let from = nested.join("from");
        let to = nested.join("to");
        fs::write(&from, b"x").unwrap();
        assert_eq!(call(&RealFileIO, "rename", vec![path(&from), path(&to)]).unwrap(), number(0));
        assert!(!from.exists() && to.is_file());
        assert_eq!(call(&RealFileIO, "delete", vec![path(&to)]).unwrap(), number(0));
        fs::write(nested.join("child"), b"x").unwrap();
        assert_eq!(call(&RealFileIO, "delete", vec![path(&nested), text("d")]).unwrap(), number(-1));
        assert_eq!(call(&RealFileIO, "delete", vec![path(&root.0.join("one")), text("rf")]).unwrap(), number(0));
        assert!(!root.0.join("one").exists());
    }

    // Test_mkdir_p (test_eval_stuff.vim 27-45): OK is 1 and FAIL is 0,
    // `prot` is validated before the flags string, and 'p' swallows
    // "already exists" only for directories.
    #[test]
    fn mkdir_reports_ok_one_fail_zero_and_validates_prot_before_flags() {
        let root = TempRoot::new("mkdir-p");
        let nested = root.0.join("Xmkdir/nested");
        assert_eq!(mkdir(&RealFileIO, &[path(&nested), text("p")], false, &mut None).unwrap(), number(1));
        assert!(nested.is_dir());
        // Existing directories with 'p' are quiet successes.
        assert_eq!(mkdir(&RealFileIO, &[path(&root.0.join("Xmkdir")), text("p")], false, &mut None).unwrap(), number(1));
        assert_eq!(mkdir(&RealFileIO, &[path(&nested), text("p")], false, &mut None).unwrap(), number(1));
        // 'p' does not suppress a real error: an existing file is E739.
        let file = root.0.join("Xfile");
        fs::write(&file, b"").unwrap();
        assert_eq!(mkdir(&RealFileIO, &[path(&file), text("p")], false, &mut None).unwrap_err().code, "E739");
        // Without 'p' an existing directory is E739 and a fresh one is 1.
        assert_eq!(mkdir(&RealFileIO, &[path(&root.0.join("Xmkdir"))], false, &mut None).unwrap_err().code, "E739");
        let fresh = root.0.join("fresh");
        assert_eq!(mkdir(&RealFileIO, &[path(&fresh)], false, &mut None).unwrap(), number(1));
        assert!(fresh.is_dir());
        // An empty name is FAIL without a message; a List name is E730.
        assert_eq!(mkdir(&RealFileIO, &[text("")], false, &mut None).unwrap(), number(0));
        assert_eq!(mkdir(&RealFileIO, &[Typval::list(Vec::new())], false, &mut None).unwrap_err().code, "E730");
        // prot (argument 3) is read as a Number before the flags string is
        // read as a String, so the List prot reports E745 and not E730.
        assert_eq!(
            mkdir(&RealFileIO, &[text("abc"), Typval::list(Vec::new()), Typval::list(Vec::new())], false, &mut None)
                .unwrap_err()
                .code,
            "E745"
        );
        assert!(!root.0.join("abc").exists(), "E745 must not create the directory");
    }

    // Test_mkdir_defer_del (test_eval_stuff.vim 69-103): 'D' defers
    // delete(dir, 'd') and 'R' defers delete(dir, 'rf'), and the deferred
    // directory is the first one the recursive walk created — not the full
    // name — when 'p' had to create parents.
    #[test]
    fn mkdir_defer_flags_report_first_created_directory_and_mode() {
        let root = TempRoot::new("mkdir-defer");
        let top = root.0.join("Xtopdir");
        assert_eq!(mkdir(&RealFileIO, &[path(&top), text("p")], false, &mut None).unwrap(), number(1));

        // Xtopdir exists, so the walk's first creation is tmp-d, not the
        // full name.
        let mut deferred = None;
        let sub = top.join("tmp-d/sub");
        assert_eq!(mkdir(&RealFileIO, &[path(&sub), text("pD")], true, &mut deferred).unwrap(), number(1));
        assert_eq!(deferred, Some((fs::canonicalize(&top.join("tmp-d")).unwrap(), DeleteMode::Dir)));

        // 'R' swaps the mode for the recursive delete; a fresh path keeps
        // this on the first-created branch.
        let mut deferred = None;
        let sub = top.join("tmp-r/sub");
        assert_eq!(mkdir(&RealFileIO, &[path(&sub), text("pR")], true, &mut deferred).unwrap(), number(1));
        assert_eq!(deferred, Some((fs::canonicalize(&top.join("tmp-r")).unwrap(), DeleteMode::Recursive)));

        // Nothing was created, so the deferred delete targets the directory
        // itself (fs.c 1143-1146).
        let mut deferred = None;
        let existing = top.join("tmp-d");
        assert_eq!(mkdir(&RealFileIO, &[path(&existing), text("pD")], true, &mut deferred).unwrap(), number(1));
        assert_eq!(deferred, Some((fs::canonicalize(&existing).unwrap(), DeleteMode::Dir)));

        // Without a function frame the D/R flags are E193 before anything is
        // created, and no defer is reported on the success path without them.
        let target = root.0.join("noframe");
        assert_eq!(mkdir(&RealFileIO, &[path(&target), text("D")], false, &mut None).unwrap_err().code, "E193");
        assert!(!target.exists(), "E193 must leave no directory behind");
        let mut deferred = None;
        assert_eq!(mkdir(&RealFileIO, &[path(&target)], true, &mut deferred).unwrap(), number(1));
        assert_eq!(deferred, None);
    }

    // Test_mkdir_defer_del (test_eval_stuff.vim 69-103) end to end: each
    // helper's frame runs its deferred delete on return, so 'D' on a
    // directory that gained contents leaves it and 'R' takes the tree with
    // it, while the script-level 'R' on Xtopdir removes the rest when
    // Suite() returns.
    #[test]
    fn mkdir_deferred_deletes_run_at_function_frame_boundaries() {
        let root = TempRoot::new("mkdir-defer-frames");
        let base = root.0.display().to_string();

        let mut editor = Editor::new();
        let mut executor = ExExecutor::new();
        executor
            .execute_script(
                &mut editor,
                "mkdir-defer.vim",
                &format!(
                    "func DoMkdirDel(name)\n\
                     call mkdir(a:name, 'pD')\n\
                     endfunc\n\
                     func DoMkdirDelAddFile(name)\n\
                     call mkdir(a:name, 'pD')\n\
                     call writefile(['text'], a:name .. '/file')\n\
                     endfunc\n\
                     func DoMkdirDelRec(name)\n\
                     call mkdir(a:name, 'pR')\n\
                     endfunc\n\
                     func DoMkdirDelRecAddFile(name)\n\
                     call mkdir(a:name, 'pR')\n\
                     call writefile(['text'], a:name .. '/file')\n\
                     endfunc\n\
                     func Suite()\n\
                     call mkdir('{base}/Xtopdir', 'R')\n\
                     call DoMkdirDel('{base}/Xtopdir/tmp')\n\
                     let g:plain = isdirectory('{base}/Xtopdir') && !isdirectory('{base}/Xtopdir/tmp')\n\
                     call DoMkdirDel('{base}/Xtopdir/tmp/sub')\n\
                     let g:contains_dir = isdirectory('{base}/Xtopdir/tmp') && isdirectory('{base}/Xtopdir/tmp/sub')\n\
                     call delete('{base}/Xtopdir/tmp', 'rf')\n\
                     call DoMkdirDelAddFile('{base}/Xtopdir/tmp')\n\
                     let g:contains_file = isdirectory('{base}/Xtopdir/tmp') && filereadable('{base}/Xtopdir/tmp/file')\n\
                     call delete('{base}/Xtopdir/tmp', 'rf')\n\
                     call DoMkdirDelRec('{base}/Xtopdir/tmp')\n\
                     let g:rec = isdirectory('{base}/Xtopdir') && !isdirectory('{base}/Xtopdir/tmp')\n\
                     call DoMkdirDelRec('{base}/Xtopdir/tmp/sub')\n\
                     let g:rec_nested = isdirectory('{base}/Xtopdir') && !isdirectory('{base}/Xtopdir/tmp')\n\
                     call DoMkdirDelRecAddFile('{base}/Xtopdir/tmp')\n\
                     let g:rec_file = isdirectory('{base}/Xtopdir') && !isdirectory('{base}/Xtopdir/tmp')\n\
                     endfunc\n\
                     call Suite()\n\
                     let g:top_after = isdirectory('{base}/Xtopdir')"
                ),
            )
            .unwrap();

        let flag = |name: &[u8]| executor.scope().get_scoped(ScopeKind::Global, name, 0).cloned();
        assert_eq!(flag(b"plain"), Ok(Typval::Number(1)), "tmp was created, so its own frame's 'D' had to remove it");
        assert_eq!(flag(b"contains_dir"), Ok(Typval::Number(1)), "'D' on tmp must fail while sub is inside it");
        assert_eq!(flag(b"contains_file"), Ok(Typval::Number(1)), "'D' on tmp must fail while file is inside it");
        assert_eq!(flag(b"rec"), Ok(Typval::Number(1)), "'R' had to remove the freshly created tmp");
        assert_eq!(flag(b"rec_nested"), Ok(Typval::Number(1)), "'R' on tmp/sub had to take tmp with it");
        assert_eq!(flag(b"rec_file"), Ok(Typval::Number(1)), "'R' had to remove tmp and its file");
        assert_eq!(flag(b"top_after"), Ok(Typval::Number(0)), "Suite's own 'R' had to remove Xtopdir");
    }

    // Test_readfile_binary (test_eval_stuff.vim 170-186) and
    // Test_readfile_binary_empty (188-193): binary keeps CRs and the
    // trailing empty item — an empty binary file is [''] — while text drops
    // both. NULs become newlines inside lines in both modes.
    #[test]
    fn readfile_binary_keeps_carriage_returns_and_trailing_empty_line() {
        let root = TempRoot::new("readfile-binary");
        let file = root.0.join("dos");
        fs::write(&file, b"one\r\ntwo\r\nthree\r\n").unwrap();
        assert_eq!(
            call(&RealFileIO, "readfile", vec![path(&file)]).unwrap(),
            Typval::list(vec![text("one"), text("two"), text("three")])
        );
        assert_eq!(
            call(&RealFileIO, "readfile", vec![path(&file), text(""), number(2)]).unwrap(),
            Typval::list(vec![text("one"), text("two")])
        );
        assert_eq!(
            call(&RealFileIO, "readfile", vec![path(&file), text("b")]).unwrap(),
            Typval::list(vec![text("one\r"), text("two\r"), text("three\r"), text("")])
        );
        assert_eq!(
            call(&RealFileIO, "readfile", vec![path(&file), text("b"), number(2)]).unwrap(),
            Typval::list(vec![text("one\r"), text("two\r")])
        );

        let empty = root.0.join("empty");
        fs::write(&empty, b"").unwrap();
        assert_eq!(call(&RealFileIO, "readfile", vec![path(&empty), text("b")]).unwrap(), Typval::list(vec![text("")]));
        assert_eq!(call(&RealFileIO, "readfile", vec![path(&empty)]).unwrap(), Typval::list(Vec::new()));

        let nulls = root.0.join("nulls");
        fs::write(&nulls, b"a\0b\nc\0\n").unwrap();
        assert_eq!(
            call(&RealFileIO, "readfile", vec![path(&nulls)]).unwrap(),
            Typval::list(vec![text("a\nb"), text("c\n")])
        );
        assert_eq!(
            call(&RealFileIO, "readfile", vec![path(&nulls), text("b")]).unwrap(),
            Typval::list(vec![text("a\nb"), text("c\n"), text("")])
        );

        // 'B' is an exact-match flag returning the whole file as a Blob;
        // "bb" is not 'b', so it reads as text.
        assert_eq!(call(&RealFileIO, "readfile", vec![path(&file), text("B")]).unwrap(), Typval::Blob(b"one\r\ntwo\r\nthree\r\n".to_vec()));
        assert_eq!(
            call(&RealFileIO, "readfile", vec![path(&file), text("bb")]).unwrap(),
            Typval::list(vec![text("one"), text("two"), text("three")])
        );
    }

    // Test_readfile_bom (test_eval_stuff.vim 195-199): text mode removes
    // EF BB BF at any position, not only at byte 0; binary keeps them.
    #[test]
    fn readfile_removes_byte_order_marks_anywhere_in_text_mode() {
        let root = TempRoot::new("readfile-bom");
        let file = root.0.join("bom");
        fs::write(&file, b"\xef\xbb\xbfFOO\nFOO\xef\xbb\xbfBAR\n").unwrap();
        assert_eq!(
            call(&RealFileIO, "readfile", vec![path(&file)]).unwrap(),
            Typval::list(vec![text("FOO"), text("FOOBAR")])
        );
        assert_eq!(
            call(&RealFileIO, "readfile", vec![path(&file), text("b")]).unwrap(),
            Typval::list(vec![text("\u{feff}FOO"), text("FOO\u{feff}BAR"), text("")])
        );
    }

    #[test]
    fn readfile_and_writefile_preserve_text_binary_and_append_contracts() {
        let root = TempRoot::new("content");
        let file = root.0.join("file");
        let lines = Typval::list(vec![text("one"), text("two")]);
        assert_eq!(write(vec![lines, path(&file)]).unwrap(), number(0));
        assert_eq!(fs::read(&file).unwrap(), b"one\ntwo\n");
        assert_eq!(call(&RealFileIO, "readfile", vec![path(&file)]).unwrap(), Typval::list(vec![text("one"), text("two")]));
        assert_eq!(write(vec![Typval::list(vec![text("three")]), path(&file), text("ab")]).unwrap(), number(0));
        assert_eq!(fs::read(&file).unwrap(), b"one\ntwo\nthree");
        assert_eq!(call(&RealFileIO, "readfile", vec![path(&file), text("b")]).unwrap(), Typval::list(vec![text("one"), text("two"), text("three")]));
        let bytes = root.0.join("bytes");
        assert_eq!(write(vec![Typval::Blob(vec![0, 0xff, b'\n']), path(&bytes)]).unwrap(), number(0));
        assert_eq!(fs::read(bytes).unwrap(), vec![0, 0xff, b'\n']);
    }

    #[test]
    fn writefile_reports_permission_and_flag_failures() {
        let root = TempRoot::new("write-errors");
        let directory = root.0.join("directory");
        fs::create_dir(&directory).unwrap();
        let error = write(vec![Typval::list(vec![text("x")]), path(&directory)]).unwrap_err();
        assert_eq!(error.code, "E482");
        let error = write(vec![Typval::list(Vec::new()), path(&root.0.join("file")), text("z")]).unwrap_err();
        assert_eq!(error.code, "E5060");
    }

    // eval/fs.c f_writefile 1835-1860 — every documented flag is accepted, and
    // `E5060` names the rest of the flag string from the offending byte.
    //
    // `D` and `p` were rejected outright before this, and `E5060: Unknown
    // flag: D` accounted for 39 oldtest files in census 3.
    //
    // One case per letter, each arranged so the other letters would give the
    // wrong answer: `p` is the only letter that creates the missing parent (the
    // same write without it must fail), `a` is the only letter that keeps the
    // previous bytes, `b` is the only letter that drops the trailing newline,
    // `s`/`S` must change nothing at all, and `D` is the only letter that needs
    // a function frame.
    #[test]
    fn writefile_accepts_every_documented_flag() {
        let root = TempRoot::new("write-flags");

        // `p` creates the parent chain; without it the same write fails.
        let deep = root.0.join("a/b/deep");
        assert_eq!(write(vec![Typval::list(vec![text("x")]), path(&deep), text("p")]).unwrap(), number(0));
        assert_eq!(fs::read(&deep).unwrap(), b"x\n");
        let deeper = root.0.join("c/d/deep");
        assert_eq!(write(vec![Typval::list(vec![text("x")]), path(&deeper)]).unwrap_err().code, "E482");

        // `a` appends, and only `a`.
        let file = root.0.join("file");
        assert_eq!(write(vec![Typval::list(vec![text("one")]), path(&file)]).unwrap(), number(0));
        assert_eq!(write(vec![Typval::list(vec![text("two")]), path(&file), text("a")]).unwrap(), number(0));
        assert_eq!(fs::read(&file).unwrap(), b"one\ntwo\n");
        assert_eq!(write(vec![Typval::list(vec![text("three")]), path(&file)]).unwrap(), number(0));
        assert_eq!(fs::read(&file).unwrap(), b"three\n");

        // `b` drops the final newline; `s` and `S` are durability controls this
        // port has no seam for and must leave the bytes exactly as `b` alone
        // would.
        let binary = root.0.join("binary");
        assert_eq!(write(vec![Typval::list(vec![text("x"), text("y")]), path(&binary), text("b")]).unwrap(), number(0));
        assert_eq!(fs::read(&binary).unwrap(), b"x\ny");
        for flags in ["s", "S", "bs", "bS"] {
            assert_eq!(write(vec![Typval::list(vec![text("x"), text("y")]), path(&binary), text(flags)]).unwrap(), number(0));
            let expected: &[u8] = if flags.contains('b') { b"x\ny" } else { b"x\ny\n" };
            assert_eq!(fs::read(&binary).unwrap(), expected, "flags {flags:?}");
        }

        // `D` needs a function frame (`can_add_defer`) and reports the path to
        // delete when it has one. The check runs before the file is opened.
        let deferred_path = root.0.join("deferred");
        let error = write(vec![Typval::list(vec![text("x")]), path(&deferred_path), text("D")]).unwrap_err();
        assert_eq!(error.code, "E193");
        assert!(!deferred_path.exists(), "E193 must leave no file behind");
        let mut deferred = None;
        assert_eq!(
            writefile(
                &RealFileIO,
                &[Typval::list(vec![text("x")]), path(&deferred_path), text("D")],
                true,
                &mut deferred,
            )
            .unwrap(),
            number(0)
        );
        assert!(deferred_path.exists());
        assert_eq!(deferred.as_deref(), Some(fs::canonicalize(&deferred_path).unwrap().as_path()));

        // An unknown letter names the remainder of the flag string, not the
        // single character: `semsg("...%s", p)`.
        let error = write(vec![Typval::list(Vec::new()), path(&file), text("bxa")]).unwrap_err();
        assert_eq!(error.code, "E5060");
        assert_eq!(error.message, "Unknown flag: xa");
    }

    #[test]
    fn glob_and_globpath_expand_recursive_wildcards_deterministically() {
        let root = TempRoot::new("glob");
        fs::create_dir_all(root.0.join("one/deep")).unwrap();
        fs::create_dir(root.0.join("two")).unwrap();
        fs::write(root.0.join("one/a.vim"), b"").unwrap();
        fs::write(root.0.join("one/deep/b.vim"), b"").unwrap();
        fs::write(root.0.join("two/c.vim"), b"").unwrap();
        let pattern = root.0.join("**/*.vim");
        let expected = Typval::list(vec![path(&root.0.join("one/a.vim")), path(&root.0.join("one/deep/b.vim")), path(&root.0.join("two/c.vim"))]);
        assert_eq!(call(&RealFileIO, "glob", vec![path(&pattern), number(0), number(1)]).unwrap(), expected);
        let paths = format!("{},{}", root.0.join("one").display(), root.0.join("two").display());
        assert_eq!(call(&RealFileIO, "globpath", vec![text(paths), text("*.vim"), number(0), number(1)]).unwrap(), Typval::list(vec![path(&root.0.join("one/a.vim")), path(&root.0.join("two/c.vim"))]));
    }

    #[test]
    fn swapfilelist_collects_swap_files_from_every_directory_entry() {
        let root = TempRoot::new("swapfilelist");
        fs::write(root.0.join(".hidden.swp"), b"").unwrap();
        fs::write(root.0.join(".hidden.swo"), b"").unwrap();
        fs::write(root.0.join(".runtest.vim.swp"), b"").unwrap();
        fs::write(root.0.join("plain"), b"").unwrap();
        fs::write(root.0.join("plain.swp"), b"").unwrap();
        let sub = root.0.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join(".nested.swp"), b"").unwrap();
        // 'directory' entries are scanned in order; per pattern (*.sw?,
        // .*.sw?, .sw?) matches follow sorted, and non-swap names never match.
        let directories = format!("{},{}", root.0.display(), sub.display());
        let expected = Typval::list(vec![
            path(&root.0.join("plain.swp")),
            path(&root.0.join(".hidden.swo")),
            path(&root.0.join(".hidden.swp")),
            path(&root.0.join(".runtest.vim.swp")),
            path(&sub.join(".nested.swp")),
        ]);
        assert_eq!(swapfilelist(&RealFileIO, 0, &directories).unwrap(), expected);
    }

    #[test]
    fn swapfilelist_current_directory_entry_yields_relative_names() {
        let _guard = crate::PROCESS_STATE_GUARD
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = TempRoot::new("swapfilelist-dot");
        fs::write(root.0.join(".one.swp"), b"").unwrap();
        fs::write(root.0.join("plain"), b"").unwrap();
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(&root.0).unwrap();
        let result = swapfilelist(&RealFileIO, 0, ".");
        std::env::set_current_dir(&previous).unwrap();
        assert_eq!(result.unwrap(), Typval::list(vec![text(".one.swp")]));
        // runtest.vim GetSwapFileList uses indexof()/remove()/delete() on the
        // returned strings, so the "./"-free form matters.
    }

    #[test]
    fn swapfilelist_rejects_arguments_and_reads_the_directory_option() {
        assert_eq!(swapfilelist(&RealFileIO, 1, ".").unwrap_err().code, "E118");
        let root = TempRoot::new("swapfilelist-option");
        fs::write(root.0.join(".opt.swp"), b"").unwrap();
        let mut editor = Editor::new();
        let mut executor = ExExecutor::new();
        executor
            .execute_script(
                &mut editor,
                "<swapfilelist-test>",
                &format!("let &directory = '{}'\nlet g:swaps = swapfilelist()", root.0.display()),
            )
            .unwrap();
        let swaps = executor
            .scope()
            .global
            .iter()
            .find(|(name, _)| name.as_bytes() == b"swaps")
            .map(|(_, value)| value.clone())
            .unwrap();
        assert_eq!(swaps, Typval::list(vec![path(&root.0.join(".opt.swp"))]));
    }

    // Plain `mkdir(..., 'p')` inside a function must not register a deferred
    // delete; the directory has to survive the frame's cleanup.
    #[test]
    fn mkdir_plain_p_survives_function_return() {
        let root = TempRoot::new("mkdir-plain-p");
        let base = root.0.display().to_string();

        let mut editor = Editor::new();
        let mut executor = ExExecutor::new();
        executor
            .execute_script(
                &mut editor,
                "mkdir-plain-p.vim",
                &format!(
                    "func MakeDir(name)\n\
                     call mkdir(a:name, 'p')\n\
                     endfunc\n\
                     call MakeDir('{base}/Xplain')\n\
                     let g:after = isdirectory('{base}/Xplain')"
                ),
            )
            .unwrap();

        assert!(root.0.join("Xplain").is_dir());
        let flag = |name: &[u8]| executor.scope().get_scoped(ScopeKind::Global, name, 0).cloned();
        assert_eq!(flag(b"after"), Ok(Typval::Number(1)));
    }

    // `:defer delete()` must forward its optional flags to the deferred call:
    // no flag removes a file, 'd' removes an empty directory, and 'rf' removes
    // a directory tree. Invalid flags raise the same E15 `delete()` does.
    #[test]
    fn defer_delete_honors_d_rf_flags_and_rejects_invalid_flags() {
        let root = TempRoot::new("defer-delete");
        let base = root.0.display().to_string();

        let mut editor = Editor::new();
        let mut executor = ExExecutor::new();
        executor
            .execute_script(
                &mut editor,
                "defer-delete.vim",
                &format!(
                    "func DeferFile(name)\n\
                     call writefile(['x'], a:name)\n\
                     defer delete(a:name)\n\
                     endfunc\n\
                     func DeferDir(name)\n\
                     call mkdir(a:name, 'p')\n\
                     defer delete(a:name, 'd')\n\
                     endfunc\n\
                     func DeferRec(name)\n\
                     call mkdir(a:name, 'p')\n\
                     call writefile(['x'], a:name .. '/file')\n\
                     defer delete(a:name, 'rf')\n\
                     endfunc\n\
                     func Suite()\n\
                     call DeferFile('{base}/file')\n\
                     let g:file_gone = !filereadable('{base}/file')\n\
                     call DeferDir('{base}/dir')\n\
                     let g:dir_gone = !isdirectory('{base}/dir')\n\
                     call DeferRec('{base}/rec')\n\
                     let g:rec_gone = !isdirectory('{base}/rec') && !filereadable('{base}/rec/file')\n\
                     endfunc\n\
                     call Suite()"
                ),
            )
            .unwrap();

        let flag = |name: &[u8]| executor.scope().get_scoped(ScopeKind::Global, name, 0).cloned();
        assert_eq!(flag(b"file_gone"), Ok(Typval::Number(1)));
        assert_eq!(flag(b"dir_gone"), Ok(Typval::Number(1)));
        assert_eq!(flag(b"rec_gone"), Ok(Typval::Number(1)));

        let mut editor = Editor::new();
        let mut executor = ExExecutor::new();
        let result = executor.execute_script(
            &mut editor,
            "defer-delete-invalid.vim",
            &format!(
                "func Bad(name)\n\
                 defer delete(a:name, 'x')\n\
                 endfunc\n\
                 call Bad('{base}/ignored')"
            ),
        );
        assert!(result.is_err(), "invalid defer delete flags must raise an error");
    }
    // `f_filecopy` copies a regular file or a symbolic link.  When a rename
    // across devices fails, `f_rename` falls back to copy-then-unlink.
    #[test]
    fn filecopy_copies_regular_file_and_symlink() {
        let root = TempRoot::new("filecopy");
        let from = root.0.join("from");
        let to = root.0.join("to");
        fs::write(&from, b"data").unwrap();
        assert_eq!(call(&RealFileIO, "filecopy", vec![path(&from), path(&to)]).unwrap(), number(1));
        assert_eq!(fs::read(&to).unwrap(), b"data");

        let link = root.0.join("link");
        let link_target = root.0.join("target");
        fs::write(&link_target, b"via link").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&link_target, &link).unwrap();
            let link_copy = root.0.join("link-copy");
            assert_eq!(call(&RealFileIO, "filecopy", vec![path(&link), path(&link_copy)]).unwrap(), number(1));
            assert!(link_copy.is_symlink() || link_copy.is_file());
        }

        // Non-string arguments report E1174 with argument position.
        let list_arg = Typval::list(Vec::new());
        let blob_arg = Typval::Blob(vec![0]);
        let error = call(&RealFileIO, "filecopy", vec![list_arg, path(&to)])
            .unwrap_err();
        assert_eq!(error.code, "E1174");
        assert!(error.message.contains("String required for argument 1"), "got: {}", error.message);
        let error = call(&RealFileIO, "filecopy", vec![path(&from), blob_arg])
            .unwrap_err();
        assert_eq!(error.code, "E1174");
        assert!(error.message.contains("String required for argument 2"), "got: {}", error.message);
    }

    // `f_readblob` only reads the whole file for `size == -1`; any other
    // non-positive size (for example -2) returns an empty blob.  A missing
    // file reports `E484`.
    #[test]
    fn readblob_negative_size_and_read_failure() {
        let root = TempRoot::new("readblob");
        let file = root.0.join("file");
        fs::write(&file, b"abcdef").unwrap();
        let whole = call(&RealFileIO, "readblob", vec![path(&file)]).unwrap();
        assert_eq!(whole, Typval::Blob(b"abcdef".to_vec()));

        let with_offset = call(&RealFileIO, "readblob", vec![path(&file), number(2)]).unwrap();
        assert_eq!(with_offset, Typval::Blob(b"cdef".to_vec()));


        let with_size = call(&RealFileIO, "readblob", vec![path(&file), number(1), number(2)]).unwrap();
        assert_eq!(with_size, Typval::Blob(b"bc".to_vec()));

        let empty = call(&RealFileIO, "readblob", vec![path(&file), number(0), number(-2)]).unwrap();
        assert_eq!(empty, Typval::Blob(Vec::new()));

        let missing = root.0.join("missing");
        let error = call(&RealFileIO, "readblob", vec![path(&missing)]).unwrap_err();
        assert_eq!(error.code, "E484");
        assert!(error.message.contains("Can't open file"), "got: {}", error.message);
    }
}
