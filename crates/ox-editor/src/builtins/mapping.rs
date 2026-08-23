//! Mapping query builtins: `maparg`.
//!
//! `f_maparg` is `get_maparg(argvars, rettv, exact = 1)` (`mapping.c:2148-2227`),
//! which resolves the left-hand side through `replace_termcodes`, finds the
//! mapping with `check_map` (`mapping.c:2010-2061`), and then answers either
//! `str2special` of the stored right-hand side or the dictionary
//! `mapblock_fill_dict` builds in its *compatible* form (`mapping.c:2090-2146`).
//!
//! The compatible form is sixteen keys, plus `lhsrawalt` when the left-hand
//! side simplified and `desc`/`callback` when the mapping carries them. `buf`
//! is deliberately absent: `mapblock_fill_dict` adds it only when `compatible`
//! is false, which is the `nvim_get_keymap` path and not this one.
//!
//! Named gaps, each one a thing that would have to exist first:
//!
//!  * **`callback`** needs a Funcref. [`MappingAction::Callback`] is a `u64`
//!    host-callback identity, not a callable value, and upstream's key holds
//!    the `LuaRef` itself (`mapping.c:2111-2112`). Nothing script-reachable
//!    creates one — `:map` cannot — so a mapping with a callback answers the
//!    empty dictionary rather than an invented Funcref.
//!  * **`lhsrawalt`** needs key simplification. Upstream emits it only when
//!    `replace_termcodes` reported `did_simplify` (`mapping.c:2124-2127`), that
//!    is when the written left-hand side has a second, simplified byte form
//!    (`<C-I>` and `<Tab>`). This port has no `m_simplified` model and no
//!    `REPTERM_NO_SIMPLIFY` pass, so it never emits the key — which is also
//!    what upstream does for every left-hand side that does not simplify.
//!  * **`abbr = 1`** needs the abbreviation table to be script-reachable and to
//!    carry mapping flags. `:abbreviate` is not an executed Ex command here, so
//!    no script can add an entry, and [`crate::mapping::Abbreviation`] records
//!    no mode set, `nowait`, `silent` or original right-hand side. The query
//!    therefore answers nothing rather than filling a dictionary from data that
//!    does not exist.
//!
//! `sid`, `lnum`, `script` and `replace_keycodes` are *not* gaps. The first two
//! come from the script stack, which [`crate::script::Scripts`] tracks; `script`
//! comes from the `<script>` flag [`crate::mapping::MappingOptions`] now
//! records; and `replace_keycodes` is only settable through
//! `nvim_set_keymap`'s option table, so upstream reports 0 for every mapping a
//! `:map` command can create, which is every mapping here.

use ox_eval::EvalError;
use ox_eval::Scope;
use ox_types::{OxStr, Typval};

use crate::excmd_exec::{map_leader, EvalHost};
use crate::mapping::{MapModes, Mapping, MappingAction};
use crate::script::FileIO;
use crate::typeahead::{special_notation, Keys};

/// Routes one mapping query builtin.
pub(crate) fn call<F: FileIO>(
    host: &mut EvalHost<'_, F>,
    name: &str,
    args: Vec<Typval>,
    scope: &Scope,
) -> ox_eval::Result<Typval> {
    match name {
        "maparg" => maparg(host, args, scope),
        _ => unreachable!("mapping builtin route and dispatcher disagree"),
    }
}

/// `get_maparg` (`mapping.c:2148-2227`).
fn maparg<F: FileIO>(
    host: &mut EvalHost<'_, F>,
    args: Vec<Typval>,
    scope: &Scope,
) -> ox_eval::Result<Typval> {
    check_arity("maparg", args.len())?;
    let keys = super::input_string_arg(&args[0])?;
    // An empty left-hand side returns the empty *string* even when a
    // dictionary was asked for: `get_maparg` returns before it reads
    // `get_dict` (`mapping.c:2155-2157`).
    if keys.0.is_empty() {
        return Ok(Typval::String(OxStr::from("")));
    }
    let which = match args.get(1) {
        Some(value) => super::input_string_arg(value)?.to_string_lossy().into_owned(),
        None => String::new(),
    };
    let abbr = args.get(2).is_some_and(Typval::is_truthy);
    let want_dict = args.get(3).is_some_and(Typval::is_truthy);
    let modes = MapModes::from_mode_string(&which);

    let empty = || {
        if want_dict { Typval::dict(Vec::new()) } else { Typval::String(OxStr::from("")) }
    };
    // See this module's header: the abbreviation table cannot answer.
    if abbr {
        return Ok(empty());
    }
    // `replace_termcodes` on the queried keys, the same translation `:map`
    // applied when the mapping was stored, so `maparg('<Esc>x')` finds what
    // `:nnoremap <Esc>x` defined.
    let lhs = Keys::parse_notation(
        &keys.to_string_lossy(),
        &map_leader(scope, "mapleader"),
        &map_leader(scope, "maplocalleader"),
    );
    let buffer = host.editor.current_buffer();
    let Some((mapping, local)) = host.editor.mappings().find_exact(lhs.as_bytes(), modes, buffer)
    else {
        return Ok(empty());
    };
    // A callback mapping has no key string, and this port cannot hand back the
    // Funcref that would replace it.
    let Some(replaced) = mapping.action.replaced_keys() else {
        return Ok(empty());
    };
    if want_dict {
        return Ok(fill_dict(mapping, local));
    }
    // `get_maparg`'s string form (`mapping.c:2200-2210`): an empty right-hand
    // side prints as the literal `<Nop>`, everything else through
    // `str2special` with neither spaces nor `<` replaced.
    Ok(Typval::String(if replaced.is_empty() {
        OxStr::from("<Nop>")
    } else {
        OxStr::from(special_notation(replaced, false, false).as_str())
    }))
}

/// `mapblock_fill_dict` with `compatible` true (`mapping.c:2090-2146`).
fn fill_dict(mapping: &Mapping, local: bool) -> Typval {
    let options = &mapping.options;
    let context = options.script_context;
    let mut entries = vec![
        // `compatible` reports `m_orig_str`, the right-hand side as written,
        // rather than `str2special` of the replaced form.
        (OxStr::from("rhs"), Typval::String(OxStr::from(options.orig_rhs.as_str()))),
        (OxStr::from("lhs"), Typval::String(OxStr::from(special_notation(mapping.lhs.as_bytes(), true, false).as_str()))),
        (OxStr::from("lhsraw"), Typval::String(OxStr(mapping.lhs.as_bytes().to_vec()))),
        // The compatible form cannot distinguish `<script>`, so `noremap` is
        // just "does not remap" (`mapping.c:2101-2104`).
        (OxStr::from("noremap"), Typval::Number(i64::from(!options.remap))),
        (OxStr::from("script"), Typval::Number(i64::from(options.script))),
        (OxStr::from("expr"), Typval::Number(i64::from(matches!(mapping.action, MappingAction::Expr(_))))),
        (OxStr::from("silent"), Typval::Number(i64::from(options.silent))),
        (OxStr::from("sid"), Typval::Number(i64::try_from(context.sid).unwrap_or(0))),
        // Hard-coded upstream too (`mapping.c:2133`).
        (OxStr::from("scriptversion"), Typval::Number(1)),
        (OxStr::from("lnum"), Typval::Number(i64::try_from(context.lnum).unwrap_or(0))),
        (OxStr::from("buffer"), Typval::Number(i64::from(local))),
        (OxStr::from("nowait"), Typval::Number(i64::from(options.nowait))),
        // Only `nvim_set_keymap`'s option table sets `m_replace_keycodes`, and
        // this port has no such surface, so every mapping reports upstream's
        // value for a `:map`-created one.
        (OxStr::from("replace_keycodes"), Typval::Number(0)),
        (OxStr::from("mode"), Typval::String(OxStr::from(options.modes.to_chars().as_str()))),
        (OxStr::from("abbr"), Typval::Number(0)),
        (OxStr::from("mode_bits"), Typval::Number(i64::from(options.modes.bits()))),
    ];
    if let Some(description) = &options.description {
        entries.push((OxStr::from("desc"), Typval::String(OxStr::from(description.as_str()))));
    }
    Typval::dict(entries)
}

/// Enforces the `eval.lua` argument counts the way upstream's function table
/// does before a builtin body runs.
fn check_arity(name: &str, count: usize) -> ox_eval::Result<()> {
    let spec = ox_eval::builtin_spec(name).expect("mapping builtins come from eval.lua");
    if count < spec.min_args {
        return Err(EvalError::new("E119", 0, format!("Not enough arguments for function: {name}")));
    }
    if spec.max_args.is_some_and(|maximum| count > maximum) {
        return Err(EvalError::new("E118", 0, format!("Too many arguments for function: {name}")));
    }
    Ok(())
}
