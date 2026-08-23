//! Pure path and filesystem builtins.
//!
//! Semantics follow `src/nvim/eval/funcs.c` and `runtime/doc/vimfn.txt`, with
//! modifier ordering from `runtime/doc/cmdline.txt` `filename-modifiers`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ox_types::{OxStr, Typval};

use crate::error::{EvalError, Result};
use crate::eval::RegexEngine;
use crate::find_file::{FindSearch, FindWhat};
use crate::scope::{OptionScope, Scope};

pub(crate) fn getcwd(args: &[Typval]) -> Result<Typval> {
    for argument in args {
        number_arg(argument)?;
    }
    let directory = std::env::current_dir()
        .map_err(|error| EvalError::new("E472", 0, error.to_string()))?;
    Ok(text(directory.to_string_lossy()))
}

pub(crate) fn is_absolute_path(value: &Typval) -> Result<Typval> {
    let value = string_arg(value)?;
    Ok(boolean(Path::new(&value.to_string_lossy().as_ref()).is_absolute()))
}

pub(crate) fn executable(value: &Typval) -> Result<Typval> {
    let program = string_arg(value)?.to_string_lossy().into_owned();
    if program.is_empty() {
        return Ok(boolean(false));
    }
    let path = Path::new(&program);
    if path.components().count() > 1 {
        return Ok(boolean(is_executable(path)));
    }
    let found = std::env::var_os("PATH").is_some_and(|search| {
        std::env::split_paths(&search).any(|directory| is_executable(&directory.join(&program)))
    });
    Ok(boolean(found))
}

pub(crate) fn exepath(value: &Typval) -> Result<Typval> {
    let program = string_arg(value)?.to_string_lossy().into_owned();
    if program.is_empty() { return Ok(text("")); }
    let path = Path::new(&program);
    if path.components().count() > 1 {
        return Ok(if is_executable(path) { text(fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()).to_string_lossy()) } else { text("") });
    }
    let found = std::env::var_os("PATH").and_then(|search| std::env::split_paths(&search).map(|directory| directory.join(&program)).find(|candidate| is_executable(candidate)));
    Ok(found.map_or_else(|| text(""), |candidate| text(fs::canonicalize(&candidate).unwrap_or(candidate).to_string_lossy())))
}

pub(crate) fn simplify(value: &Typval) -> Result<Typval> {
    let input = string_arg(value)?;
    Ok(text(simplify_name(&input.to_string_lossy())))
}

pub(crate) fn resolve(value: &Typval) -> Result<Typval> {
    let input = string_arg(value)?.to_string_lossy().into_owned();
    let trailing_separator = input.ends_with('/');
    let leading_dot = input.starts_with("./");
    let path = Path::new(&input);
    let mut output = if let Ok(canonical) = fs::canonicalize(path) {
        if path.is_absolute() {
            canonical.to_string_lossy().into_owned()
        } else if let Ok(current) = std::env::current_dir() {
            canonical
                .strip_prefix(current)
                .unwrap_or(&canonical)
                .to_string_lossy()
                .into_owned()
        } else {
            canonical.to_string_lossy().into_owned()
        }
    } else {
        simplify_name(&input)
    };
    if leading_dot && !path.is_absolute() && !output.starts_with("./") {
        output.insert_str(0, "./");
    }
    if trailing_separator && !output.ends_with('/') {
        output.push('/');
    }
    Ok(text(output))
}

pub(crate) fn fnamemodify(
    regex: Option<&dyn RegexEngine>,
    filename: &Typval,
    modifiers: &Typval,
) -> Result<Typval> {
    let mut name = string_arg(filename)?.to_string_lossy().into_owned();
    let modifiers = string_arg(modifiers)?;
    let bytes = modifiers.as_bytes();
    let mut cursor = 0;

    while bytes.get(cursor) == Some(&b':') {
        cursor += 1;
        let Some(modifier) = bytes.get(cursor).copied() else {
            break;
        };
        cursor += 1;
        match modifier {
            b'8' => {}
            b'p' => name = absolute_name(&name),
            b'~' => name = relative_to_home(&name),
            b'.' => name = relative_to_current(&name),
            b'h' => name = path_head(&name),
            b't' => name = path_tail(&name),
            b'r' => name = path_root(&name),
            b'e' => {
                let mut count = 1;
                while bytes.get(cursor..cursor + 2) == Some(b":e") {
                    count += 1;
                    cursor += 2;
                }
                name = path_extension(&name, count);
            }
            b's' | b'g' => {
                let global = modifier == b'g';
                if global {
                    if bytes.get(cursor) != Some(&b's') {
                        break;
                    }
                    cursor += 1;
                }
                let Some(delimiter) = bytes.get(cursor).copied() else {
                    break;
                };
                cursor += 1;
                let Some((pattern, next)) = modifier_part(bytes, cursor, delimiter) else {
                    break;
                };
                cursor = next;
                let Some((replacement, next)) = modifier_part(bytes, cursor, delimiter) else {
                    break;
                };
                cursor = next;
                let engine = regex.ok_or_else(|| {
                    EvalError::new("E54", 0, "regular-expression engine is not installed")
                })?;
                let flags = if global { OxStr::from("g") } else { OxStr::from("") };
                name = engine
                    .substitute(
                        &OxStr(name.into_bytes()),
                        &OxStr(pattern.into_bytes()),
                        &OxStr(replacement.into_bytes()),
                        &flags,
                    )?
                    .to_string_lossy()
                    .into_owned();
            }
            b'S' => name = shell_escape(&name),
            _ => break,
        }
    }

    Ok(text(name))
}

/// `tempname()` — `f_tempname` (`eval/fs.c:1701-1705`) calling
/// `vim_tempname` (`fileio.c:3588-3603`): a unique, not-yet-created name
/// inside a private directory this process owns, numbered from 0.
///
/// Errors when no candidate root yields a private directory, which is
/// upstream's `vim_gettempdir() == NULL`; upstream returns an empty string
/// there, but it has already logged the reason, so the reason is reported.
pub(crate) fn tempname() -> Result<Typval> {
    static TEMPDIR: LazyLock<Option<PathBuf>> = LazyLock::new(make_tempdir);
    static COUNT: AtomicU64 = AtomicU64::new(0);

    let Some(directory) = TEMPDIR.as_ref() else {
        return Err(EvalError::new("E5431", 0, "cannot create a temporary directory"));
    };
    let count = COUNT.fetch_add(1, Ordering::Relaxed);
    // `vim_settempdir` stores the directory with a trailing separator and
    // `vim_tempname` concatenates the counter directly onto it.
    Ok(text(directory.join(count.to_string()).to_string_lossy()))
}

/// `vim_mktempdir` (`fileio.c:3303-3396`) followed by `vim_settempdir`:
/// walk `TEMP_DIR_NAMES`, create `nvim.<user>` mode 0700 under the first
/// existing root, drop the `<user>` component when that directory is not a
/// private directory we own, then `mkdtemp` inside it.
fn make_tempdir() -> Option<PathBuf> {
    let user = tempdir_user();
    for root in temp_dir_names() {
        if !root.is_dir() {
            continue;
        }
        let owned_root = root.join(format!("nvim.{user}"));
        // Always create, to avoid a race, then verify it is ours.
        create_private_dir(&owned_root);
        let parent = if is_private_dir(&owned_root) { owned_root } else { root };
        let Some(created) = mkdtemp(&parent) else { continue };
        // `vim_FullName` so a later `:cd` cannot change the meaning.
        return Some(fs::canonicalize(&created).unwrap_or(created));
    }
    None
}

/// `TEMP_DIR_NAMES` (`os/unix_defs.h:17`) with `expand_env` applied.
fn temp_dir_names() -> Vec<PathBuf> {
    let mut roots = Vec::with_capacity(4);
    if let Some(value) = std::env::var_os("TMPDIR") {
        roots.push(PathBuf::from(value));
    }
    roots.push(PathBuf::from("/tmp"));
    roots.push(PathBuf::from("."));
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home));
    }
    roots
}

/// `os_get_username` (`os/users.c`): the `/etc/passwd` name of the real uid,
/// or the uid rendered as a decimal number when it has none. Upstream then
/// replaces path separators, because a user name may contain them.
fn tempdir_user() -> String {
    let uid = current_uid();
    let name = uid.and_then(passwd_name).unwrap_or_else(|| uid.map_or_else(|| "0".to_owned(), |uid| uid.to_string()));
    name.replace(['/', '\\'], "_")
}

/// The real uid from `/proc/self/status`, which is how this crate already
/// reads process and kernel state (see `hostname()`).
fn current_uid() -> Option<u32> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let field = status.lines().find_map(|line| line.strip_prefix("Uid:"))?;
    field.split_whitespace().next()?.parse().ok()
}

/// The `getpwuid` name for `uid`, read from `/etc/passwd`.
fn passwd_name(uid: u32) -> Option<String> {
    let passwd = fs::read_to_string("/etc/passwd").ok()?;
    for line in passwd.lines() {
        let mut fields = line.split(':');
        let name = fields.next()?;
        let _password = fields.next();
        if fields.next().and_then(|value| value.parse::<u32>().ok()) == Some(uid) && !name.is_empty() {
            return Some(name.to_owned());
        }
    }
    None
}

/// `os_mkdir(path, 0700)`. Upstream lowers the umask around the whole of
/// `vim_mktempdir` instead; setting the mode explicitly reaches the same
/// permissions without touching process-wide state.
#[cfg(unix)]
fn create_private_dir(path: &Path) -> bool {
    use std::os::unix::fs::DirBuilderExt as _;
    fs::DirBuilder::new().mode(0o700).create(path).is_ok()
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> bool {
    fs::create_dir(path).is_ok()
}

/// `isdir && os_file_owned() && 0700 == (perm & 0777)` (`fileio.c:3342-3346`).
#[cfg(unix)]
fn is_private_dir(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    use std::os::unix::fs::PermissionsExt as _;
    let Ok(metadata) = fs::metadata(path) else { return false };
    metadata.is_dir()
        && current_uid() == Some(metadata.uid())
        && metadata.permissions().mode() & 0o777 == 0o700
}

#[cfg(not(unix))]
fn is_private_dir(path: &Path) -> bool {
    path.is_dir()
}

/// `os_mkdtemp(parent/XXXXXX)`: create a fresh private directory, retrying
/// on collision. The candidate name follows `ox_uv::fs`'s scheme so the two
/// temporary-path generators stay recognizably the same shape.
fn mkdtemp(parent: &Path) -> Option<PathBuf> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    for _ in 0..1024 {
        let sequence = u128::from(SEQUENCE.fetch_add(1, Ordering::Relaxed));
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_nanos();
        let name = format!("{:06x}", (stamp ^ sequence ^ u128::from(std::process::id())) & 0xff_ffff);
        let candidate = parent.join(name);
        if create_private_dir(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// `findfile()`/`finddir()` — `findfilendir` (`eval/fs.c:542-605`).
///
/// `{path}` defaults to the buffer-local `'path'`, then the global one, then
/// upstream's compiled default `.,,`; `'suffixesadd'` is consulted for files
/// only. A negative `{count}` collects every match into a List, and any
/// other value returns the `{count}`-th match as a String, where 0 and 1
/// both mean the first.
pub(crate) fn findfilendir(
    regex: Option<&dyn RegexEngine>,
    args: &[Typval],
    scope: &Scope,
    find_what: FindWhat,
) -> Result<Typval> {
    let name = string_arg(&args[0])?.to_string_lossy().into_owned();
    let mut path = option_list(scope, b"path", ".,,");
    let mut count = 1i64;
    if let Some(value) = args.get(1) {
        let given = string_arg(value)?.to_string_lossy().into_owned();
        if !given.is_empty() {
            path = given;
        }
        if let Some(value) = args.get(2) {
            count = number_arg(value)?;
        }
    }
    let as_list = count < 0;
    if name.is_empty() {
        return Ok(if as_list { Typval::list(Vec::new()) } else { text("") });
    }

    let suffixes = match find_what {
        FindWhat::Dir => String::new(),
        FindWhat::File => option_list(scope, b"suffixesadd", ""),
    };
    let regex = regex.ok_or_else(|| EvalError::new("E54", 0, "regular-expression engine is not installed"))?;
    let mut search = FindSearch::new(regex, &name, &path, &suffixes, find_what);

    if as_list {
        let mut found = Vec::new();
        while let Some(result) = search.next_match()? {
            found.push(text(result));
        }
        return Ok(Typval::list(found));
    }
    // Upstream keeps only the last result of the loop, so asking for more
    // matches than exist yields an empty string rather than an earlier hit.
    let mut result;
    let mut remaining = count;
    loop {
        result = search.next_match()?;
        remaining -= 1;
        if remaining <= 0 || result.is_none() {
            break;
        }
    }
    Ok(result.map_or_else(|| text(""), text))
}

/// A comma-separated option value: the buffer-local value when it is not
/// empty, else the global one, else upstream's compiled default. A `Scope`
/// with no entry for the option is a host that never set it.
fn option_list(scope: &Scope, name: &[u8], default: &str) -> String {
    for option_scope in [OptionScope::Local, OptionScope::Global] {
        if !scope.contains_option(option_scope, name) {
            continue;
        }
        if let Typval::String(value) = scope.get_option(option_scope, name) {
            if !value.as_bytes().is_empty() {
                return value.to_string_lossy().into_owned();
            }
        }
    }
    default.to_owned()
}

fn boolean(value: bool) -> Typval {
    Typval::Number(i64::from(value))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

fn text(value: impl AsRef<str>) -> Typval {
    Typval::String(OxStr::from(value.as_ref()))
}

fn string_arg(value: &Typval) -> Result<OxStr> {
    match value {
        Typval::String(value) => Ok(value.clone()),
        Typval::Number(value) => Ok(OxStr(value.to_string().into_bytes())),
        Typval::Bool(value) => Ok(OxStr::from(if *value { "v:true" } else { "v:false" })),
        Typval::Special(ox_types::Special::Null) => Ok(OxStr::from("")),
        Typval::Float(_) => Err(EvalError::new("E806", 0, "Using a Float as a String")),
        Typval::List(_) => Err(EvalError::new("E730", 0, "Using a List as a String")),
        Typval::Dict(_) => Err(EvalError::new("E731", 0, "Using a Dictionary as a String")),
        _ => Err(EvalError::new("E729", 0, "Using invalid value as a String")),
    }
}

fn number_arg(value: &Typval) -> Result<i64> {
    match value {
        Typval::Number(value) => Ok(*value),
        Typval::Bool(value) => Ok(i64::from(*value)),
        Typval::Special(ox_types::Special::Null) => Ok(0),
        Typval::String(value) => Ok(parse_integer_prefix(value.as_bytes()).unwrap_or(0)),
        Typval::Float(_) => Err(EvalError::new("E805", 0, "Using a Float as a Number")),
        Typval::List(_) => Err(EvalError::new("E745", 0, "Using a List as a Number")),
        Typval::Dict(_) => Err(EvalError::new("E728", 0, "Using a Dictionary as a Number")),
        _ => Err(EvalError::new("E745", 0, "Using invalid value as a Number")),
    }
}

fn parse_integer_prefix(bytes: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(bytes).ok()?.trim_start();
    let end = text
        .char_indices()
        .take_while(|(index, character)| character.is_ascii_digit() || (*index == 0 && matches!(character, '+' | '-')))
        .map(|(index, character)| index + character.len_utf8())
        .last()?;
    text[..end].parse().ok()
}

fn absolute_name(name: &str) -> String {
    let expanded = expand_home(name);
    let path = Path::new(&expanded);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut output = fs::canonicalize(&absolute)
        .unwrap_or_else(|_| PathBuf::from(simplify_name(&absolute.to_string_lossy())))
        .to_string_lossy()
        .into_owned();
    if absolute.is_dir() && !output.ends_with('/') {
        output.push('/');
    }
    output
}

fn expand_home(name: &str) -> String {
    if (name == "~" || name.starts_with("~/")) && std::env::var_os("HOME").is_some() {
        let home = PathBuf::from(std::env::var_os("HOME").expect("checked above"));
        return format!("{}{}", home.to_string_lossy(), &name[1..]);
    }
    name.to_owned()
}

fn relative_to_home(name: &str) -> String {
    let Some(home) = std::env::var_os("HOME") else {
        return name.to_owned();
    };
    relative_with_prefix(name, &PathBuf::from(home), "~")
}

fn relative_to_current(name: &str) -> String {
    let Ok(current) = std::env::current_dir() else {
        return name.to_owned();
    };
    relative_with_prefix(name, &current, "")
}

fn relative_with_prefix(name: &str, base: &Path, prefix: &str) -> String {
    match Path::new(name).strip_prefix(base) {
        Ok(relative) if relative.as_os_str().is_empty() => prefix.to_owned(),
        Ok(relative) if prefix.is_empty() => relative.to_string_lossy().into_owned(),
        Ok(relative) => format!("{prefix}/{}", relative.to_string_lossy()),
        Err(_) => name.to_owned(),
    }
}

fn path_head(name: &str) -> String {
    if name.is_empty() {
        return ".".to_owned();
    }
    if name.len() > 1 && name.ends_with('/') {
        return name.trim_end_matches('/').to_owned();
    }
    match name.rfind('/') {
        Some(0) => "/".to_owned(),
        Some(index) => name[..index].to_owned(),
        None => String::new(),
    }
}

fn path_tail(name: &str) -> String {
    if name.ends_with('/') {
        String::new()
    } else {
        name.rsplit('/').next().unwrap_or(name).to_owned()
    }
}

fn tail_dot(name: &str) -> Option<usize> {
    let tail = name.rsplit('/').next().unwrap_or(name);
    let dot = tail.rfind('.')?;
    (dot > 0).then_some(name.len() - tail.len() + dot)
}

fn path_root(name: &str) -> String {
    tail_dot(name).map_or_else(|| name.to_owned(), |dot| name[..dot].to_owned())
}

fn path_extension(name: &str, count: usize) -> String {
    let tail = name.rsplit('/').next().unwrap_or(name);
    let dots: Vec<usize> = tail
        .match_indices('.')
        .map(|(index, _)| index)
        .filter(|index| *index > 0)
        .collect();
    if dots.is_empty() {
        return String::new();
    }
    tail[dots[dots.len().saturating_sub(count)] + 1..].to_owned()
}

fn modifier_part(bytes: &[u8], start: usize, delimiter: u8) -> Option<(String, usize)> {
    let mut output = Vec::new();
    let mut cursor = start;
    while cursor < bytes.len() {
        if bytes[cursor] == delimiter {
            return Some((String::from_utf8_lossy(&output).into_owned(), cursor + 1));
        }
        if bytes[cursor] == b'\\' && bytes.get(cursor + 1) == Some(&delimiter) {
            cursor += 1;
        }
        output.push(bytes[cursor]);
        cursor += 1;
    }
    None
}

fn shell_escape(name: &str) -> String {
    format!("'{}'", name.replace('\'', "'\\''"))
}

pub(crate) fn simplify_name(name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    let absolute = name.starts_with('/');
    let double_root = name.starts_with("//") && !name.starts_with("///");
    let leading_dot = name.starts_with("./");
    let trailing_separator = name.ends_with('/');
    let mut collapsed_to_current = false;
    let mut parts: Vec<&str> = Vec::new();
    for component in name.split('/') {
        match component {
            "" | "." => {}
            ".." if parts.last().is_some_and(|part| *part != "..") => {
                parts.pop();
                collapsed_to_current |= parts.is_empty();
            }
            ".." if !absolute => parts.push(component),
            ".." => {}
            _ => parts.push(component),
        }
    }
    let mut output = parts.join("/");
    if absolute {
        output.insert_str(0, if double_root { "//" } else { "/" });
    } else if (leading_dot || collapsed_to_current) && !output.is_empty() && !output.starts_with("../") && output != ".." {
        output.insert_str(0, "./");
    }
    if output.is_empty() {
        output = if absolute {
            "/"
        } else if leading_dot {
            "./"
        } else {
            "."
        }
        .to_owned();
    }
    if trailing_separator && !output.ends_with('/') {
        output.push('/');
    }
    output
}
