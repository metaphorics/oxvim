//! Filesystem-backed Vimscript builtins routed exclusively through [`FileIO`].
//!
//! Semantics follow Neovim `src/nvim/eval/fs.c`: `f_delete` (438-470),
//! metadata functions (527-539, 834-887), `f_glob`/`f_globpath` (924-1014),
//! `f_swapfilelist` (7200) via `recover_names` (memline.c 1303-1429),
//! `f_mkdir` (1087-1140), `read_file_or_blob` (1299-1496), `f_rename`
//! (1512-1521), and `f_writefile`/`write_list` (1714-1760, 1802-1906).

use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use ox_eval::{builtin_spec, EvalError};
use ox_types::{OxStr, Typval};

use crate::script::{FileIO, FileKind};

pub(crate) fn is_filesystem_builtin(name: &str) -> bool {
    matches!(
        name,
        "mkdir" | "delete" | "rename" | "glob" | "globpath" | "readfile" | "writefile"
            | "filereadable" | "isdirectory" | "getftime" | "getfsize" | "getfperm"
            | "filewritable"
    )
}

pub(crate) fn call(io: &dyn FileIO, name: &str, args: Vec<Typval>) -> ox_eval::Result<Typval> {
    check_arity(name, args.len())?;
    match name {
        "mkdir" => mkdir(io, &args),
        "delete" => delete(io, &args),
        "rename" => rename(io, &args),
        "glob" => glob(io, &args),
        "globpath" => globpath(io, &args),
        "readfile" => readfile(io, &args),
        "writefile" => writefile(io, &args),
        "filereadable" => filereadable(io, &args[0]),
        "isdirectory" => isdirectory(io, &args[0]),
        "getftime" => getftime(io, &args[0]),
        "getfsize" => getfsize(io, &args[0]),
        "getfperm" => getfperm(io, &args[0]),
        "filewritable" => filewritable(io, &args[0]),
        _ => unreachable!("filesystem builtin predicate and dispatcher disagree"),
    }
}

fn check_arity(name: &str, count: usize) -> ox_eval::Result<()> {
    let spec = builtin_spec(name).ok_or_else(|| EvalError::not_implemented(OxStr::from(name)))?;
    if count < spec.min_args {
        return Err(EvalError::new("E119", 0, format!("Not enough arguments for function: {name}")));
    }
    if spec.max_args.is_some_and(|maximum| count > maximum) {
        return Err(EvalError::new("E118", 0, format!("Too many arguments for function: {name}")));
    }
    Ok(())
}

fn mkdir(io: &dyn FileIO, args: &[Typval]) -> ox_eval::Result<Typval> {
    let path = path_arg(&args[0])?;
    if path.as_os_str().is_empty() {
        return Ok(number(-1));
    }
    let flags = optional_string(args.get(1))?;
    let recursive = flags.as_deref().is_some_and(|value| value.contains('p'));
    let mode = args.get(2).map(number_arg).transpose()?.unwrap_or(0o755);
    if mode < 0 {
        return Ok(number(-1));
    }
    io.create_dir(&path, recursive, mode as u32)
        .map(|()| number(0))
        .map_err(|error| EvalError::new("E739", 0, format!("Cannot create directory {path:?}: {error}")))
}

fn delete(io: &dyn FileIO, args: &[Typval]) -> ox_eval::Result<Typval> {
    let path = path_arg(&args[0])?;
    if path.as_os_str().is_empty() {
        return Err(EvalError::new("E474", 0, "Invalid argument"));
    }
    let flags = optional_string(args.get(1))?.unwrap_or_default();
    let result = match flags.as_str() {
        "" => io.remove_file(&path),
        "d" => io.remove_dir(&path),
        "rf" => io.remove_dir_all(&path),
        _ => return Err(EvalError::new("E15", 0, format!("Invalid expression: {flags}"))),
    };
    Ok(number(if result.is_ok() { 0 } else { -1 }))
}

fn rename(io: &dyn FileIO, args: &[Typval]) -> ox_eval::Result<Typval> {
    let from = path_arg(&args[0])?;
    let to = path_arg(&args[1])?;
    Ok(number(if io.rename(&from, &to).is_ok() { 0 } else { -1 }))
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

fn readfile(io: &dyn FileIO, args: &[Typval]) -> ox_eval::Result<Typval> {
    let path = path_arg(&args[0])?;
    let kind = optional_string(args.get(1))?.unwrap_or_default();
    let binary = kind.contains('b');
    let maximum = args.get(2).map(number_arg).transpose()?;
    let mut bytes = io.read_bytes(&path)
        .map_err(|error| EvalError::new("E484", 0, format!("Can't open file {path:?}: {error}")))?;
    if !binary && bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        bytes.drain(..3);
    }
    let ended_in_newline = bytes.last() == Some(&b'\n');
    let mut lines: Vec<Vec<u8>> = bytes.split(|byte| *byte == b'\n').map(|line| {
        let mut line = line.to_vec();
        if !binary && line.last() == Some(&b'\r') { line.pop(); }
        for byte in &mut line { if *byte == 0 { *byte = b'\n'; } }
        line
    }).collect();
    if bytes.is_empty() { lines.clear(); }
    else if !binary && ended_in_newline { lines.pop(); }
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

fn writefile(io: &dyn FileIO, args: &[Typval]) -> ox_eval::Result<Typval> {
    let path = path_arg(&args[1])?;
    if path.as_os_str().is_empty() {
        return Err(EvalError::new("E482", 0, "Can't open file with an empty name"));
    }
    let flags = optional_string(args.get(2))?.unwrap_or_default();
    for flag in flags.chars() {
        if !matches!(flag, 'b' | 'a' | 's' | 'S') {
            return Err(EvalError::new("E5060", 0, format!("Unknown flag: {flag}")));
        }
    }
    let binary = flags.contains('b');
    let append = flags.contains('a');
    let bytes = write_data(&args[0], binary)?;
    io.write_bytes(&path, &bytes, append)
        .map(|()| number(0))
        .map_err(|error| EvalError::new("E482", 0, format!("Can't open file {path:?} for writing: {error}")))
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

fn expand_glob(io: &dyn FileIO, pattern: &str, all_links: bool) -> Vec<String> {
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

fn wildcard_match(pattern: &[u8], value: &[u8]) -> bool {
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
        assert_eq!(call(&RealFileIO, "mkdir", vec![path(&nested), text("p"), number(0o700)]).unwrap(), number(0));
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

    #[test]
    fn readfile_and_writefile_preserve_text_binary_and_append_contracts() {
        let root = TempRoot::new("content");
        let file = root.0.join("file");
        let lines = Typval::list(vec![text("one"), text("two")]);
        assert_eq!(call(&RealFileIO, "writefile", vec![lines, path(&file)]).unwrap(), number(0));
        assert_eq!(fs::read(&file).unwrap(), b"one\ntwo\n");
        assert_eq!(call(&RealFileIO, "readfile", vec![path(&file)]).unwrap(), Typval::list(vec![text("one"), text("two")]));
        assert_eq!(call(&RealFileIO, "writefile", vec![Typval::list(vec![text("three")]), path(&file), text("ab")]).unwrap(), number(0));
        assert_eq!(fs::read(&file).unwrap(), b"one\ntwo\nthree");
        assert_eq!(call(&RealFileIO, "readfile", vec![path(&file), text("b")]).unwrap(), Typval::list(vec![text("one"), text("two"), text("three")]));
        let bytes = root.0.join("bytes");
        assert_eq!(call(&RealFileIO, "writefile", vec![Typval::Blob(vec![0, 0xff, b'\n']), path(&bytes)]).unwrap(), number(0));
        assert_eq!(fs::read(bytes).unwrap(), vec![0, 0xff, b'\n']);
    }

    #[test]
    fn writefile_reports_permission_and_flag_failures() {
        let root = TempRoot::new("write-errors");
        let directory = root.0.join("directory");
        fs::create_dir(&directory).unwrap();
        let error = call(&RealFileIO, "writefile", vec![Typval::list(vec![text("x")]), path(&directory)]).unwrap_err();
        assert_eq!(error.code, "E482");
        let error = call(&RealFileIO, "writefile", vec![Typval::list(Vec::new()), path(&root.0.join("file")), text("z")]).unwrap_err();
        assert_eq!(error.code, "E5060");
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
        static CWD_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = CWD_GUARD.lock().unwrap_or_else(|poison| poison.into_inner());
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
}
