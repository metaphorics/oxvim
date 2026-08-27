//! `:set`-style merging of an option value.
//!
//! `nvim_set_option_value()` accepts `operation = "append" | "prepend" |
//! "remove"`, which upstream turns into the same `OP_ADDING`/`OP_PREPENDING`/
//! `OP_REMOVING` merge `:set +=`, `:set ^=` and `:set -=` use
//! (api/options.c `nvim_set_option_value` → option.c `get_option_newval`).
//! `vim.opt.X:append()` and friends are written on top of it, so the whole
//! `vim.opt` compound API depends on this file.

use ox_editor::{OptionListKind, OptionMetadata, OptionType, OptionValue};
use ox_types::ApiError;

/// The `operation` key of `nvim_set_option_value` (upstream `set_op_T`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SetOp {
    /// `:set=`, replacing the value.
    Set,
    /// `:set+=`.
    Append,
    /// `:set^=`.
    Prepend,
    /// `:set-=`.
    Remove,
}

impl SetOp {
    /// Parses the `operation` key, rejecting anything else the way
    /// api/options.c `validate_option_value_args` does.
    pub(crate) fn parse(text: &[u8]) -> Result<Self, ApiError> {
        match text {
            b"set" => Ok(Self::Set),
            b"append" => Ok(Self::Append),
            b"prepend" => Ok(Self::Prepend),
            b"remove" => Ok(Self::Remove),
            _ => Err(ApiError::validation(
                "Invalid 'operation': expected 'set', 'append', 'prepend', or 'remove'",
            )),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Set => "set",
            Self::Append => "append",
            Self::Prepend => "prepend",
            Self::Remove => "remove",
        }
    }

    /// Boolean options have nothing to merge into, which upstream reports as a
    /// conflict rather than a type error.
    pub(crate) fn check_supported(self, metadata: &OptionMetadata) -> Result<(), ApiError> {
        if self != Self::Set && metadata.value_type == OptionType::Boolean {
            return Err(ApiError::validation(format!(
                "Conflict: '{}' not allowed with boolean options",
                self.name()
            )));
        }
        Ok(())
    }
}

/// The `:set` grammar bits of one option that the merge depends on, named after
/// the upstream `kOptFlag*` bits they stand in for.
struct ListFlags {
    /// `kOptFlagComma`: items are comma separated.
    comma: bool,
    /// `kOptFlagOneComma`: items are comma separated and never empty.
    one_comma: bool,
    /// `kOptFlagColon`: items may take a `key:value` form.
    colon: bool,
    /// `kOptFlagFlagList`: the value is a sequence of single-character flags.
    flag_list: bool,
    /// `kOptFlagNoDup`: an item already present is not added again.
    nodup: bool,
}

fn list_flags(metadata: &OptionMetadata) -> ListFlags {
    let (comma, one_comma, colon, flag_list) = match metadata.list {
        None => (false, false, false, false),
        Some(OptionListKind::Comma) => (true, false, false, false),
        Some(OptionListKind::OneComma) => (true, true, false, false),
        Some(OptionListKind::CommaColon) => (true, false, true, false),
        Some(OptionListKind::OneCommaColon) => (true, true, true, false),
        Some(OptionListKind::Flags) => (false, false, false, true),
        Some(OptionListKind::FlagsComma) => (true, false, false, true),
    };
    ListFlags { comma, one_comma, colon, flag_list, nodup: metadata.deny_duplicates }
}

/// option.c `get_option_newval`: folds `value` into the option's `current`
/// value. Numbers add, multiply and subtract; strings follow the `:set` string
/// merge. `Set` returns `value` untouched.
pub(crate) fn merge(
    metadata: &'static OptionMetadata,
    current: &OptionValue,
    value: OptionValue,
    op: SetOp,
) -> Result<OptionValue, ApiError> {
    if op == SetOp::Set {
        return Ok(value);
    }
    match (current, value) {
        (OptionValue::Number(current), OptionValue::Number(value)) => {
            Ok(OptionValue::Number(match op {
                SetOp::Append => current.saturating_add(value),
                SetOp::Prepend => current.saturating_mul(value),
                SetOp::Remove => current.saturating_sub(value),
                SetOp::Set => value,
            }))
        }
        (OptionValue::String(current), OptionValue::String(value)) => {
            Ok(OptionValue::String(merge_string(&list_flags(metadata), current, &value, op)))
        }
        (_, value) => Err(ApiError::validation(format!(
            "Invalid '{}': expected a valid type, got {}",
            metadata.name,
            match value {
                OptionValue::Boolean(_) => "Boolean",
                OptionValue::Number(_) => "Integer",
                OptionValue::String(_) => "String",
            }
        ))),
    }
}

/// option.c `stropt_get_newval` minus the `:set` command-line unescaping, which
/// does not apply to a value that arrived as an API string.
fn merge_string(flags: &ListFlags, origval: &str, newval: &str, op: SetOp) -> String {
    if flags.comma
        && flags.colon
        && let Some(merged) = merge_key_items(origval, newval, op)
    {
        return finish(flags, merged);
    }

    // Locate `newval` inside `origval`, both to remove it and to avoid adding a
    // duplicate. An item already present makes adding a no-op.
    let mut removal = origval.len();
    if op == SetOp::Remove || flags.nodup {
        match find_dup_item(origval, newval, flags.comma) {
            Some(found) => {
                if matches!(op, SetOp::Append | SetOp::Prepend) {
                    return finish(flags, origval.to_owned());
                }
                removal = found;
            }
            None => removal = origval.len(),
        }
    }

    let merged = match op {
        SetOp::Append | SetOp::Prepend => concat_with_comma(flags, origval, newval, op),
        SetOp::Remove => remove_value(flags, origval, removal, newval.len()),
        SetOp::Set => newval.to_owned(),
    };
    finish(flags, merged)
}

fn finish(flags: &ListFlags, value: String) -> String {
    if flags.flag_list { remove_dup_flags(flags, value) } else { value }
}

/// option.c `stropt_concat_with_comma`.
fn concat_with_comma(flags: &ListFlags, origval: &str, newval: &str, op: SetOp) -> String {
    let comma = flags.comma && !origval.is_empty() && !newval.is_empty();
    let mut merged = String::with_capacity(origval.len() + newval.len() + 1);
    if op == SetOp::Append {
        // A trailing comma would otherwise double up.
        let bytes = origval.as_bytes();
        let mut keep = origval.len();
        if comma && keep > 1 && flags.one_comma && bytes[keep - 1] == b',' && bytes[keep - 2] != b'\\' {
            keep -= 1;
        }
        merged.push_str(&origval[..keep]);
        if comma {
            merged.push(',');
        }
        merged.push_str(newval);
    } else {
        merged.push_str(newval);
        if comma {
            merged.push(',');
        }
        merged.push_str(origval);
    }
    merged
}

/// option.c `stropt_remove_val`: drop `len` bytes at `at`, plus the comma that
/// separated the removed item from its neighbour. `at` past the end means the
/// value was not found, and nothing is removed.
fn remove_value(flags: &ListFlags, origval: &str, at: usize, len: usize) -> String {
    if at >= origval.len() {
        return origval.to_owned();
    }
    let (mut at, mut len) = (at, len);
    if flags.comma {
        if at == 0 {
            if origval.as_bytes().get(len) == Some(&b',') {
                len += 1;
            }
        } else {
            at -= 1;
            len += 1;
        }
    }
    let mut merged = String::with_capacity(origval.len());
    merged.push_str(&origval[..at]);
    merged.push_str(&origval[(at + len).min(origval.len())..]);
    merged
}

/// option.c `find_dup_item`: the offset of `newval` in `origval`, required to
/// start and end on item boundaries for a comma-separated option. Only a comma
/// preceded by an even number of backslashes separates items.
fn find_dup_item(origval: &str, newval: &str, comma: bool) -> Option<usize> {
    let haystack = origval.as_bytes();
    let needle = newval.as_bytes();
    let mut backslashes = 0usize;
    for index in 0..haystack.len() {
        let starts_item = !comma || index == 0 || (haystack[index - 1] == b',' && backslashes % 2 == 0);
        if starts_item
            && haystack[index..].starts_with(needle)
            && (!comma || matches!(haystack.get(index + needle.len()), None | Some(b',')))
        {
            return Some(index);
        }
        if (index > 1 && haystack[index - 1] == b'\\' && haystack[index - 2] != b',')
            || (index == 1 && haystack[0] == b'\\')
        {
            backslashes += 1;
        } else {
            backslashes = 0;
        }
    }
    None
}

/// option.c `stropt_remove_dupflags`: a flag character may appear only once.
fn remove_dup_flags(flags: &ListFlags, value: String) -> String {
    let mut bytes = value.into_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if flags.one_comma {
            if bytes[index] != b','
                && bytes.get(index + 1) == Some(&b',')
                && bytes[index + 2..].contains(&bytes[index])
            {
                bytes.drain(index..index + 2);
                continue;
            }
        } else if (!flags.comma || bytes[index] != b',')
            && bytes[index + 1..].contains(&bytes[index])
        {
            bytes.remove(index);
            continue;
        }
        index += 1;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// option.c `stropt_handle_keymatch`: for a comma-separated option whose items
/// may be `key:value`, every item of `newval` is merged on its key rather than
/// on the whole item, so `fillchars:append('fold:.')` replaces an existing
/// `fold:-` instead of adding a second entry for the same key. Returns `None`
/// when `newval` is one plain item, which upstream routes through the ordinary
/// string merge instead.
fn merge_key_items(origval: &str, newval: &str, op: SetOp) -> Option<String> {
    if !newval.contains(':') && !newval.contains(',') {
        return None;
    }
    let mut merged = origval.to_owned();
    for item in newval.split(',').filter(|item| !item.is_empty()) {
        match item.find(':') {
            Some(colon) => {
                let key = &item[..=colon];
                if op == SetOp::Remove {
                    remove_key_item(&mut merged, key, None);
                    continue;
                }
                match find_key_item(&merged, key, 0) {
                    Some((at, len)) if merged[at..at + len] == *item => {
                        // Exact duplicate: keep it where it is, drop the rest.
                        remove_key_item(&mut merged, key, Some(at));
                    }
                    Some(_) => {
                        remove_key_item(&mut merged, key, None);
                        insert_item(&mut merged, item, op);
                    }
                    None => insert_item(&mut merged, item, op),
                }
            }
            None => match (op, find_dup_item(&merged, item, true)) {
                (SetOp::Remove, Some(at)) => remove_comma_item(&mut merged, at, item.len()),
                (SetOp::Remove, None) => {}
                (_, Some(_)) => {}
                (_, None) => insert_item(&mut merged, item, op),
            },
        }
    }
    Some(merged)
}

/// option.c `append_item`/`prepend_item`.
fn insert_item(value: &mut String, item: &str, op: SetOp) {
    if value.is_empty() {
        value.push_str(item);
    } else if op == SetOp::Prepend {
        value.insert(0, ',');
        value.insert_str(0, item);
    } else {
        value.push(',');
        value.push_str(item);
    }
}

/// option.c `find_key_item`: the first item at or after `from` whose text
/// starts with `key` (the `name:` prefix), as an offset and item length.
fn find_key_item(value: &str, key: &str, from: usize) -> Option<(usize, usize)> {
    let bytes = value.as_bytes();
    for index in from..bytes.len() {
        if (index == 0 || bytes[index - 1] == b',') && value[index..].starts_with(key) {
            let len = value[index..].find(',').unwrap_or(value.len() - index);
            return Some((index, len));
        }
    }
    None
}

/// option.c `remove_key_item`: drop every item with this key, except the one
/// starting at `skip`. Every removal is after `skip`, so its offset holds.
fn remove_key_item(value: &mut String, key: &str, skip: Option<usize>) {
    loop {
        let Some((mut at, mut len)) = find_key_item(value, key, 0) else { return };
        if skip == Some(at) {
            let mut next = at + len;
            if value.as_bytes().get(next) == Some(&b',') {
                next += 1;
            }
            match find_key_item(value, key, next) {
                Some(found) => (at, len) = found,
                None => return,
            }
        }
        remove_comma_item(value, at, len);
    }
}

/// option.c `remove_comma_item`: drop the item and the comma that joined it to
/// its neighbour, preferring the trailing one.
fn remove_comma_item(value: &mut String, at: usize, len: usize) {
    let bytes = value.as_bytes();
    if bytes.get(at + len) == Some(&b',') {
        value.replace_range(at..at + len + 1, "");
    } else if at > 0 && bytes[at - 1] == b',' {
        value.replace_range(at - 1..at + len, "");
    } else {
        value.truncate(at);
    }
}

/// option.c `option_expand` via `stropt_expand_envvar`: options flagged
/// `expand` substitute `$VAR`, `${VAR}` and a `~` that opens the value or a
/// comma-separated item. An unset variable is left standing, as `vim_getenv`
/// returning NULL leaves it. Upstream skips this only when merging into an
/// option that is not comma-separated.
pub(crate) fn expand_value(metadata: &OptionMetadata, op: SetOp, value: &str) -> String {
    if !metadata.expand || (op != SetOp::Set && metadata.list.is_none()) {
        return value.to_owned();
    }
    let home = std::env::var_os("HOME");
    let bytes = value.as_bytes();
    let mut expanded: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        // `~` only opens a path at the start of the value or of a list item.
        let item_start = index == 0 || bytes[index - 1] == b',';
        match bytes[index] {
            b'~' if item_start
                && matches!(bytes.get(index + 1), None | Some(b'/'))
                && let Some(home) = home.as_deref() =>
            {
                expanded.extend_from_slice(home.to_string_lossy().as_bytes());
                index += 1;
            }
            b'$' => {
                let braced = bytes.get(index + 1) == Some(&b'{');
                let name_start = index + if braced { 2 } else { 1 };
                let mut end = name_start;
                while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                    end += 1;
                }
                let closed = !braced || bytes.get(end) == Some(&b'}');
                let name = &value[name_start..end];
                match std::env::var_os(name).filter(|_| closed && !name.is_empty()) {
                    Some(text) => {
                        expanded.extend_from_slice(text.to_string_lossy().as_bytes());
                        index = if braced { end + 1 } else { end };
                    }
                    None => {
                        expanded.push(b'$');
                        index += 1;
                    }
                }
            }
            byte => {
                expanded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&expanded).into_owned()
}
