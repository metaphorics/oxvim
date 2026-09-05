// Pure unit-test module: unwrap/expect/panic on eval results IS the
// assertion; a failed unwrap here is a test failure, not recoverable.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Behavioral cases drawn from `runtime/doc/builtin.txt` and oldtests.

use std::cell::Cell;

use ox_types::{OxStr, Special, Typval};

use crate::builtins::{BUILTINS, Builtins};
use crate::error::EvalErrorKind;
use crate::eval::{BuiltinHost, Evaluator, NoRegex, RegexEngine};
use crate::parser::Parser;
use crate::scope::{Scope, ScopeKind};

fn text(value: &str) -> Typval {
    Typval::String(OxStr::from(value))
}
fn number(value: i64) -> Typval {
    Typval::Number(value)
}
fn list(values: &[i64]) -> Typval {
    Typval::list(values.iter().copied().map(Typval::Number).collect())
}
fn funcref(name: &str) -> Typval {
    Typval::Funcref(ox_types::Funcref {
        name: OxStr::from(name),
        args: vec![],
        dict: None,
        registry: None,
    })
}
fn call(name: &str, args: Vec<Typval>) -> crate::Result<Typval> {
    let mut builtins = Builtins::without_regex();
    builtins.call(&OxStr::from(name), args, &mut Scope::new())
}

// `runtime/doc/builtin.txt` function sections and
// `test/old/testdir/test_functions.vim` are the oracle for these table cases.
macro_rules! case {
    ($name:ident, $function:literal, [$($arg:expr),* $(,)?], $expected:expr) => {
        #[doc = concat!("Oracle: `runtime/doc/builtin.txt` `", $function, "()` section.")]
        #[test]
        fn $name() {
            assert_eq!(call($function, vec![$($arg),*]).unwrap(), $expected);
        }
    };
}

case!(abs_positive, "abs", [number(7)], number(7));
case!(abs_negative, "abs", [number(-7)], number(7));
case!(abs_float, "abs", [Typval::Float(-1.5)], Typval::Float(1.5));
case!(bit_and, "and", [number(6), number(3)], number(2));
case!(bit_or, "or", [number(4), number(3)], number(7));

/// Restores one process environment binding on drop: a value present before
/// the test is written back, absence is restored by unsetting.
struct EnvGuard {
    name: std::ffi::OsString,
    prior: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn take(name: impl Into<std::ffi::OsString>) -> Self {
        let name = name.into();
        let prior = std::env::var_os(&name);
        Self { name, prior }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prior {
            Some(value) => {
                ox_sys::set_env(&self.name, value);
            }
            None => {
                ox_sys::unset_env(&self.name);
            }
        }
    }
}

case!(bit_xor, "xor", [number(7), number(3)], number(4));
case!(
    ceil_fraction,
    "ceil",
    [Typval::Float(1.2)],
    Typval::Float(2.0)
);
case!(
    floor_fraction,
    "floor",
    [Typval::Float(1.8)],
    Typval::Float(1.0)
);
case!(sqrt_square, "sqrt", [number(9)], Typval::Float(3.0));
case!(
    pow_integer_inputs,
    "pow",
    [number(2), number(3)],
    Typval::Float(8.0)
);
case!(
    float2nr_positive,
    "float2nr",
    [Typval::Float(3.9)],
    number(3)
);
case!(
    float2nr_negative,
    "float2nr",
    [Typval::Float(-3.9)],
    number(-3)
);
// `Test_trunc` (`test_float_func.vim:305-308`) is `string(trunc(2.1)) ==
// '2.0'`, so the answer is a Float. These two asserted a Number, which is
// what `trunc` gave while it shared `float2nr`'s dispatch arm.
case!(
    trunc_positive,
    "trunc",
    [Typval::Float(4.8)],
    Typval::Float(4.0)
);
case!(
    trunc_negative,
    "trunc",
    [Typval::Float(-4.8)],
    Typval::Float(-4.0)
);
case!(empty_zero, "empty", [number(0)], number(1));
case!(empty_nonzero, "empty", [number(1)], number(0));
case!(empty_string, "empty", [text("")], number(1));
case!(empty_nonempty_string, "empty", [text("x")], number(0));
case!(empty_list, "empty", [list(&[])], number(1));
case!(empty_dict, "empty", [Typval::dict(vec![])], number(1));
case!(len_string_bytes, "len", [text("abc")], number(3));
case!(len_list, "len", [list(&[1, 2, 3])], number(3));
case!(
    len_dict,
    "len",
    [Typval::dict(vec![(OxStr::from("a"), number(1))])],
    number(1)
);
case!(strlen_unicode_bytes, "strlen", [text("é")], number(2));
case!(strcharlen_unicode, "strcharlen", [text("é")], number(1));
case!(strchars_ascii, "strchars", [text("abc")], number(3));
case!(toupper_ascii, "toupper", [text("aBc")], text("ABC"));
case!(tolower_ascii, "tolower", [text("aBc")], text("abc"));
case!(trim_default, "trim", [text("  a \n")], text("a"));
case!(trim_mask, "trim", [text("xxabcxx"), text("x")], text("abc"));
case!(
    trim_left,
    "trim",
    [text("xxabcxx"), text("x"), number(1)],
    text("abcxx")
);
case!(
    trim_right,
    "trim",
    [text("xxabcxx"), text("x"), number(2)],
    text("xxabc")
);
case!(
    trim_unicode_mask,
    "trim",
    [text("你好RESERVE好你"), text("你好")],
    text("RESERVE")
);
case!(
    trim_default_control_and_nbsp,
    "trim",
    [text("\u{000b}\u{00a0}x\u{00a0}\u{000b}")],
    text("x")
);
case!(
    toupper_uses_simple_case_mapping,
    "toupper",
    [text("ẖǰŉẗẘẙ")],
    text("ẖǰŉẗẘẙ")
);
case!(
    tolower_dotted_i_is_single_character,
    "tolower",
    [text("İ")],
    text("i")
);
case!(
    translate_unicode_characters,
    "tr",
    [text("cab"), text("abc"), text("xyz")],
    text("zxy")
);
case!(
    strwidth_counts_wide_and_combining,
    "strwidth",
    [text("a界e\u{301}")],
    number(4)
);
case!(
    strtrans_control_and_zero_width,
    "strtrans",
    [text("a\tb\u{200b}")],
    text("a^Ib<200b>")
);
case!(
    strutf16len_ignores_composing_by_default,
    "strutf16len",
    [text("-a\u{301}-b\u{301}")],
    number(4)
);
case!(
    strutf16len_counts_composing_when_requested,
    "strutf16len",
    [text("-a\u{301}-b\u{301}"), Typval::Bool(true)],
    number(6)
);
case!(
    charidx_groups_composing_marks,
    "charidx",
    [text("xa\u{301}b\u{301}y"), number(3)],
    number(1)
);
case!(
    charidx_counts_composing_when_requested,
    "charidx",
    [text("xa\u{301}b\u{301}y"), number(4), number(1)],
    number(3)
);
case!(
    charidx_accepts_utf16_offsets,
    "charidx",
    [
        text("a😊b"),
        number(2),
        Typval::Bool(false),
        Typval::Bool(true)
    ],
    number(1)
);
case!(
    utf16idx_maps_byte_inside_supplementary,
    "utf16idx",
    [text("a😊b"), number(3)],
    number(1)
);
case!(
    utf16idx_maps_character_offsets,
    "utf16idx",
    [
        text("a😊b"),
        number(2),
        Typval::Bool(false),
        Typval::Bool(true)
    ],
    number(3)
);
case!(
    pathshorten_preserves_prefixes,
    "pathshorten",
    [text("~.foo/bar/baz"), number(2)],
    text("~.fo/ba/baz")
);
case!(
    keytrans_printable_names,
    "keytrans",
    [text(" <|\\")],
    text("<Space><lt><Bar><Bslash>")
);

#[test]
fn keytrans_preserves_named_and_modified_escapes() {
    let expression = Parser::new(r#"keytrans("\<Tab>\<C-V>\<M-π>")"#.as_bytes())
        .parse()
        .unwrap();
    let mut builtins = Builtins::without_regex();
    assert_eq!(
        Evaluator::new(&mut builtins, &NoRegex)
            .eval(&expression, &mut Scope::new())
            .unwrap(),
        text("<Tab><C-V><M-π>")
    );
}

#[test]
fn sha256_hashes_strings_and_blobs_with_lowercase_hex() {
    // Oracle: test/old/testdir/test_sha256.vim Test_sha256().
    let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    assert_eq!(call("sha256", vec![text("")]).unwrap(), text(empty));
    assert_eq!(
        call("sha256", vec![text("abc")]).unwrap(),
        text("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
    );
    // `\n` stores 0x0A in both implementations, so the oracle vector applies
    // to the literal this evaluator lexes.
    assert_eq!(
        call("sha256", vec![text("foo\nbar")]).unwrap(),
        text("807eff6267f3f926a21d234f7b0cf867a86f47e07a532f15e8cc39ed110ca776")
    );
    assert_eq!(
        call("sha256", vec![Typval::Blob(vec![0xde, 0xad, 0xbe, 0xef])]).unwrap(),
        text("5f78c33274e43fa9de5659265c1d917e25c03722dcb0b8d27db8d5feaa813953")
    );
    // The empty blob hashes zero bytes like the empty string.
    assert_eq!(
        call("sha256", vec![Typval::Blob(vec![])]).unwrap(),
        text(empty)
    );
    // Upstream's method form: "abc"->sha256().
    let expression = Parser::new(br#""abc"->sha256()"#).parse().unwrap();
    let mut builtins = Builtins::without_regex();
    assert_eq!(
        Evaluator::new(&mut builtins, &NoRegex)
            .eval(&expression, &mut Scope::new())
            .unwrap(),
        text("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
    );
}

#[test]
fn sha256_cuts_strings_at_nul_and_hashes_blobs_whole() {
    // `f_sha256` (`eval/funcs.c:6653-6654`) measures string input with
    // `strlen`, so a NUL byte ends the hashed bytes; a Blob keeps its length
    // and hashes whole (`funcs.c:6649-6651`).
    let embedded = Typval::String(OxStr(b"a\0bc".to_vec()));
    assert_eq!(
        call("sha256", vec![embedded]).unwrap(),
        call("sha256", vec![text("a")]).unwrap()
    );
    // A NUL byte inside a blob is content: the three-byte input hashes whole.
    // No vector in test_sha256.vim carries an embedded NUL (Vimscript string
    // literals cut at NUL before hashing), so this digest was checked against
    // an independent SHA-256 implementation of the same three bytes.
    assert_eq!(
        call("sha256", vec![Typval::Blob(vec![0x61, 0x00, 0x62])]).unwrap(),
        text("59b271ae1bbcb1d31d41929817f4b16fb439eb4f31520b5ad1d5ce98920a7138")
    );
    // Non-blob arguments coerce through `tv_get_string` semantics.
    assert_eq!(
        call("sha256", vec![number(0)]).unwrap(),
        call("sha256", vec![text("0")]).unwrap()
    );
    assert_eq!(call("sha256", vec![list(&[1])]).unwrap_err().code, "E730");
}

#[test]
fn sha256_takes_exactly_one_argument() {
    assert_eq!(call("sha256", vec![]).unwrap_err().code, "E119");
    assert_eq!(
        call("sha256", vec![text("a"), text("b")]).unwrap_err().code,
        "E118"
    );
}

#[test]
fn setenv_sets_numeric_value_and_null_unsets() {
    const NAME: &str = "OXVIM_TEST_EVAL_SETENV";
    let _guard = EnvGuard::take(NAME);
    assert_eq!(
        call("setenv", vec![text(NAME), number(123)]).unwrap(),
        number(0)
    );
    assert_eq!(
        std::env::var_os(NAME).as_deref(),
        Some(std::ffi::OsStr::new("123"))
    );
    assert_eq!(
        call("setenv", vec![text(NAME), Typval::Special(Special::Null)]).unwrap(),
        number(0)
    );
    assert_eq!(std::env::var_os(NAME), None);
}

#[cfg(unix)]
#[test]
fn environment_builtins_preserve_non_utf8_bytes() {
    use std::os::unix::ffi::OsStringExt;

    let name_bytes = b"OXVIM_TEST_EVAL_BYTES_\xff".to_vec();
    let name = std::ffi::OsString::from_vec(name_bytes.clone());
    let _guard = EnvGuard::take(name.clone());
    let name_arg = Typval::String(OxStr(name_bytes.clone()));
    let value = OxStr(vec![0xff, 0xfe]);

    assert_eq!(
        call(
            "setenv",
            vec![name_arg.clone(), Typval::String(value.clone())],
        )
        .unwrap(),
        number(0)
    );
    assert_eq!(
        std::env::var_os(&name).unwrap().as_encoded_bytes(),
        value.as_bytes()
    );
    assert_eq!(
        call("getenv", vec![name_arg]).unwrap(),
        Typval::String(value.clone())
    );

    let Typval::Dict(environment) = call("environ", Vec::new()).unwrap() else {
        panic!("environ() must return a Dict")
    };
    assert!(environment.borrow().entries.iter().any(|entry| {
        entry.key.as_bytes() == name_bytes && entry.value == Typval::String(value.clone())
    }));
}

/// `setenv()` must be visible to `$VAR` in the same session. Upstream has no
/// environment snapshot: `f_setenv` is `os_setenv` and every `$VAR` read is
/// an `os_getenv`, so `call setenv('X', 'v')` then `echo $X` prints `v`.
/// oxvim reads `$VAR` live too. Verified against nvim v0.13.0-dev-1390.
#[test]
fn setenv_is_visible_to_environment_reads_in_the_same_scope() {
    const NAME: &str = "OXVIM_TEST_EVAL_SETENV_READBACK";
    let _guard = EnvGuard::take(NAME);
    let mut builtins = Builtins::without_regex();
    let mut scope = Scope::new();
    let read = Parser::new(format!("${NAME}").as_bytes()).parse().unwrap();

    // Absent to begin with: `$UNSET` is the empty string, not an error.
    assert_eq!(
        Evaluator::new(&mut builtins, &NoRegex)
            .eval(&read, &mut scope)
            .unwrap(),
        text("")
    );

    builtins
        .call(
            &OxStr::from("setenv"),
            vec![text(NAME), text("live")],
            &mut scope,
        )
        .unwrap();
    assert_eq!(
        Evaluator::new(&mut builtins, &NoRegex)
            .eval(&read, &mut scope)
            .unwrap(),
        text("live")
    );

    // Unsetting clears the process binding, so a stale value cannot survive.
    builtins
        .call(
            &OxStr::from("setenv"),
            vec![text(NAME), Typval::Special(Special::Null)],
            &mut scope,
        )
        .unwrap();
    assert_eq!(
        Evaluator::new(&mut builtins, &NoRegex)
            .eval(&read, &mut scope)
            .unwrap(),
        text("")
    );
    assert_eq!(std::env::var_os(NAME), None);
}

case!(
    join_default,
    "join",
    [Typval::list(vec![text("a"), text("b")])],
    text("a b")
);
case!(
    join_custom,
    "join",
    [Typval::list(vec![text("a"), text("b")]), text(",")],
    text("a,b")
);
case!(
    repeat_string,
    "repeat",
    [text("ab"), number(3)],
    text("ababab")
);
case!(
    repeat_string_zero,
    "repeat",
    [text("ab"), number(0)],
    text("")
);
case!(
    repeat_list,
    "repeat",
    [list(&[1, 2]), number(2)],
    list(&[1, 2, 1, 2])
);
case!(reverse_string, "reverse", [text("abc")], text("cba"));
case!(reverse_unicode, "reverse", [text("aé")], text("éa"));
case!(
    reverse_list,
    "reverse",
    [list(&[1, 2, 3])],
    list(&[3, 2, 1])
);
case!(
    stridx_found,
    "stridx",
    [text("abcdef"), text("cd")],
    number(2)
);
case!(
    stridx_missing,
    "stridx",
    [text("abcdef"), text("xy")],
    number(-1)
);
case!(stridx_empty, "stridx", [text("abc"), text("")], number(0));
case!(
    strridx_found,
    "strridx",
    [text("ababa"), text("ba")],
    number(3)
);
case!(
    strridx_bounded,
    "strridx",
    [text("a,b,c"), text(","), number(2)],
    number(1)
);
case!(
    strridx_missing,
    "strridx",
    [text("ababa"), text("x")],
    number(-1)
);
case!(
    strpart_middle,
    "strpart",
    [text("abcdef"), number(2), number(3)],
    text("cde")
);
case!(
    strpart_past_end,
    "strpart",
    [text("abc"), number(9), number(2)],
    text("")
);
case!(
    escape_chars,
    "escape",
    [text("a.b"), text(".")],
    text("a\\.b")
);
case!(
    add_list,
    "add",
    [list(&[1, 2]), number(3)],
    list(&[1, 2, 3])
);
case!(
    add_blob,
    "add",
    [Typval::Blob(vec![1, 2]), number(3)],
    Typval::Blob(vec![1, 2, 3])
);
case!(copy_number, "copy", [number(4)], number(4));
case!(copy_list, "copy", [list(&[1, 2])], list(&[1, 2]));
case!(
    deepcopy_nested,
    "deepcopy",
    [Typval::list(vec![list(&[1])])],
    Typval::list(vec![list(&[1])])
);
case!(
    count_list,
    "count",
    [list(&[1, 2, 1]), number(1)],
    number(2)
);
case!(count_string, "count", [text("aaaa"), text("aa")], number(2));
case!(get_list, "get", [list(&[4, 5]), number(1)], number(5));
case!(
    get_list_negative,
    "get",
    [list(&[4, 5]), number(-1)],
    number(5)
);
case!(
    get_list_default,
    "get",
    [list(&[4]), number(9), number(7)],
    number(7)
);
case!(
    get_blob,
    "get",
    [Typval::Blob(vec![8]), number(0)],
    number(8)
);
case!(
    get_dict,
    "get",
    [Typval::dict(vec![(OxStr::from("k"), number(9))]), text("k")],
    number(9)
);
case!(
    has_key_true,
    "has_key",
    [Typval::dict(vec![(OxStr::from("k"), number(1))]), text("k")],
    number(1)
);
case!(
    has_key_false,
    "has_key",
    [Typval::dict(vec![]), text("k")],
    number(0)
);
// `runtime/doc/builtin.txt` `has()`: the `nvim-X.Y[.Z]` probe compares
// against the version this build targets (0.13.0, matching
// `ox_rpc::metadata::API_LEVEL = 15`); anything beyond that target is 0.
case!(
    has_nvim_current_minor,
    "has",
    [text("nvim-0.13")],
    number(1)
);
case!(
    has_nvim_current_patch,
    "has",
    [text("nvim-0.13.0")],
    number(1)
);
case!(
    has_nvim_older_release,
    "has",
    [text("nvim-0.10")],
    number(1)
);
case!(has_nvim_newer_minor, "has", [text("nvim-0.14")], number(0));
case!(
    has_nvim_newer_patch,
    "has",
    [text("nvim-0.13.1")],
    number(0)
);
case!(has_nvim_newer_major, "has", [text("nvim-1.0")], number(0));
case!(
    has_nvim_extra_component,
    "has",
    [text("nvim-0.13.0.1")],
    number(0)
);
case!(has_nvim_not_a_version, "has", [text("nvim-dev")], number(0));
case!(has_multi_byte, "has", [text("multi_byte")], number(1));
case!(
    has_unknown_feature,
    "has",
    [text("bogus-feature")],
    number(0)
);

// Feature probes. `has()` must answer for *this* build, so each name below is
// pinned in the direction the capability was observed in, against
// `.references/neovim/build/bin/nvim` as the oracle for the question and
// against oxvim itself for the answer. `f_has` compares with `STRICMP`, so the
// probe is case-insensitive.
case!(has_is_case_insensitive, "has", [text("EVAL")], number(1));
case!(
    has_nvim_prefix_is_case_insensitive,
    "has",
    [text("NVIM-0.13")],
    number(1)
);
case!(has_eval, "has", [text("eval")], number(1));
case!(has_lambda, "has", [text("lambda")], number(1));
case!(has_float, "has", [text("float")], number(1));
case!(has_num64, "has", [text("num64")], number(1));
case!(
    has_multi_byte_encoding,
    "has",
    [text("multi_byte_encoding")],
    number(1)
);
case!(has_vimscript_1, "has", [text("vimscript-1")], number(1));
case!(has_modify_fname, "has", [text("modify_fname")], number(1));
case!(has_file_in_path, "has", [text("file_in_path")], number(1));
case!(has_path_extra, "has", [text("path_extra")], number(1));
case!(has_user_commands, "has", [text("user_commands")], number(1));
case!(
    has_user_commands_legacy_spelling,
    "has",
    [text("user-commands")],
    number(1)
);
case!(has_windows, "has", [text("windows")], number(1));
case!(has_vertsplit, "has", [text("vertsplit")], number(1));
case!(has_visual, "has", [text("visual")], number(1));
case!(has_textobjects, "has", [text("textobjects")], number(1));
case!(has_nvim, "has", [text("nvim")], number(1));
case!(has_startuptime, "has", [text("startuptime")], number(1));
case!(has_quickfix_present, "has", [text("quickfix")], number(1));
case!(has_diff_present, "has", [text("diff")], number(1));

// Subsystems upstream always compiles in that this build does not have. Each
// name stays 0 because a test that stopped skipping would run against a
// missing subsystem; the omission is recorded in
// `.outline/sdd/reports/task-63.md` with the call that reports
// `not implemented`.
case!(has_conceal_absent, "has", [text("conceal")], number(0));
case!(has_spell_absent, "has", [text("spell")], number(0));
case!(has_syntax_absent, "has", [text("syntax")], number(0));
case!(has_signs_absent, "has", [text("signs")], number(0));
case!(has_timers_absent, "has", [text("timers")], number(0));
case!(has_reltime_absent, "has", [text("reltime")], number(0));
case!(has_profile_absent, "has", [text("profile")], number(0));
case!(has_menu_absent, "has", [text("menu")], number(0));
case!(has_mksession_absent, "has", [text("mksession")], number(0));
case!(has_digraphs_absent, "has", [text("digraphs")], number(0));
case!(
    has_cmdline_hist_absent,
    "has",
    [text("cmdline_hist")],
    number(0)
);
case!(has_langmap_absent, "has", [text("langmap")], number(0));
case!(has_vartabs_absent, "has", [text("vartabs")], number(0));
case!(has_arabic_absent, "has", [text("arabic")], number(0));
case!(has_folding_absent, "has", [text("folding")], number(0));
case!(has_iconv_absent, "has", [text("iconv")], number(0));
case!(has_libcall_absent, "has", [text("libcall")], number(0));
case!(
    has_byte_offset_absent,
    "has",
    [text("byte_offset")],
    number(0)
);
case!(
    has_persistent_undo_absent,
    "has",
    [text("persistent_undo")],
    number(0)
);
case!(has_packages_absent, "has", [text("packages")], number(0));
case!(has_autocmd_absent, "has", [text("autocmd")], number(0));
case!(has_gettext_absent, "has", [text("gettext")], number(0));
case!(has_shada_absent, "has", [text("shada")], number(0));
case!(has_python3_absent, "has", [text("python3")], number(0));

/// `has("linux")`/`has("fname_case")` are the `#ifdef __linux__` and
/// `#ifndef CASE_INSENSITIVE_FILENAME` rows of `has_list`.
#[test]
fn has_platform_traits_match_the_target() {
    assert_eq!(
        call("has", vec![text("linux")]).unwrap(),
        number(i64::from(cfg!(target_os = "linux")))
    );
    assert_eq!(
        call("has", vec![text("fname_case")]).unwrap(),
        number(i64::from(cfg!(not(any(target_os = "macos", windows)))))
    );
}

/// The table `has()` answers from must stay sorted: the lookup is a
/// `binary_search`, so an out-of-order entry silently answers 0.
#[test]
fn has_answers_every_feature_it_claims() {
    for spec in crate::builtins::FEATURES {
        assert_eq!(
            call("has", vec![text(spec)]).unwrap(),
            number(1),
            "has({spec:?})"
        );
    }
}

// Capability proofs for the features answered 1 above: `has()` returning 1
// with nothing behind it is the defect these guard against.
//
// `has("file_in_path")` and `has("path_extra")` are proven by
// `findfile_and_finddir_match_upstream_over_the_oldtest_tree` below, which
// pins comma-separated 'path' entries, `**`, `**{count}` and upward `;`
// search against the oracle. The editor-side names (`user_commands`,
// `windows`, `vertsplit`, `visual`, `textobjects`) are proven at process
// level in `crates/oxvim/tests/cli.rs`.

/// `has("eval")`, `has("lambda")` and `has("vimscript-1")`: the expression
/// evaluator parses and runs Vimscript, including a lambda applied to
/// arguments. `eval()` itself needs the evaluating host rather than the
/// typval-only dispatcher, so it is proven at process level in
/// `crates/oxvim/tests/cli.rs`.
#[test]
fn eval_and_lambda_capabilities_back_their_feature_answers() {
    let mut builtins = Builtins::without_regex();
    let mut scope = Scope::new();
    let mut run = |source: &[u8]| {
        let program = Parser::new(source).parse().unwrap();
        Evaluator::new(&mut builtins, &NoRegex)
            .eval(&program, &mut scope)
            .unwrap()
    };
    assert_eq!(run(b"1 + 2"), number(3));
    assert_eq!(run(b"{a, b -> a * b}(6, 7)"), number(42));
}

/// `has("float")`: the float type exists and arithmetic, conversion and the
/// float builtins operate on it.
#[test]
fn float_capability_backs_its_feature_answer() {
    let mut builtins = Builtins::without_regex();
    let mut scope = Scope::new();
    let program = Parser::new(b"1.5 * 2.0").parse().unwrap();
    assert_eq!(
        Evaluator::new(&mut builtins, &NoRegex)
            .eval(&program, &mut scope)
            .unwrap(),
        Typval::Float(3.0)
    );
    assert_eq!(
        call("str2float", vec![text("2.5e1")]).unwrap(),
        Typval::Float(25.0)
    );
    assert_eq!(
        call("float2nr", vec![Typval::Float(3.9)]).unwrap(),
        number(3)
    );
    assert_eq!(
        call("sqrt", vec![Typval::Float(2.0)]).unwrap(),
        Typval::Float(std::f64::consts::SQRT_2)
    );
}

/// `has("num64")`: numbers are 64-bit, so a value past 2^31 survives
/// arithmetic instead of wrapping.
#[test]
fn num64_capability_backs_its_feature_answer() {
    let mut builtins = Builtins::without_regex();
    let mut scope = Scope::new();
    let program = Parser::new(b"4611686018427387904 + 1").parse().unwrap();
    assert_eq!(
        Evaluator::new(&mut builtins, &NoRegex)
            .eval(&program, &mut scope)
            .unwrap(),
        number(4_611_686_018_427_387_905)
    );
}

/// `has("multi_byte")`/`has("multi_byte_encoding")`: text is UTF-8 and the
/// character and byte lengths of the same string differ accordingly.
#[test]
fn multi_byte_capability_backs_its_feature_answer() {
    assert_eq!(call("strchars", vec![text("héllo")]).unwrap(), number(5));
    assert_eq!(call("strlen", vec![text("héllo")]).unwrap(), number(6));
    assert_eq!(call("char2nr", vec![text("é")]).unwrap(), number(233));
    assert_eq!(call("nr2char", vec![number(233)]).unwrap(), text("é"));
}

/// `has("modify_fname")`: `fnamemodify()` applies the `:h`/`:t`/`:r`
/// modifiers rather than returning its argument.
#[test]
fn modify_fname_capability_backs_its_feature_answer() {
    assert_eq!(
        call("fnamemodify", vec![text("/a/b/c.txt"), text(":t:r")]).unwrap(),
        text("c")
    );
    assert_eq!(
        call("fnamemodify", vec![text("/a/b/c.txt"), text(":h")]).unwrap(),
        text("/a/b")
    );
    assert_eq!(
        call("fnamemodify", vec![text("/a/b/c.txt"), text(":e")]).unwrap(),
        text("txt")
    );
}

/// `has("unix")`/`has("win32")`/`has("macunix")` mirror the target family the
/// binary was compiled for (`f_has` in eval/funcs.c).
#[test]
fn has_platform_matches_target_family() {
    assert_eq!(
        call("has", vec![text("unix")]).unwrap(),
        number(i64::from(cfg!(unix)))
    );
    assert_eq!(
        call("has", vec![text("win32")]).unwrap(),
        number(i64::from(cfg!(windows)))
    );
    assert_eq!(
        call("has", vec![text("macunix")]).unwrap(),
        number(i64::from(cfg!(target_os = "macos")))
    );
}

/// `has()` rejects a zero-argument call with E119 per its eval.lua arity row.
#[test]
fn has_rejects_missing_argument() {
    let error = call("has", vec![]).unwrap_err();
    assert_eq!(error.kind, EvalErrorKind::Vim);
    assert_eq!(error.code, "E119");
}
case!(
    index_found,
    "index",
    [list(&[4, 5, 4]), number(5)],
    number(1)
);
case!(
    index_missing,
    "index",
    [list(&[4, 5]), number(9)],
    number(-1)
);
case!(
    insert_front,
    "insert",
    [list(&[2, 3]), number(1)],
    list(&[1, 2, 3])
);
case!(
    insert_middle,
    "insert",
    [list(&[1, 3]), number(2), number(1)],
    list(&[1, 2, 3])
);
case!(
    keys_dict,
    "keys",
    [Typval::dict(vec![
        (OxStr::from("a"), number(1)),
        (OxStr::from("b"), number(2))
    ])],
    Typval::list(vec![text("a"), text("b")])
);
case!(
    values_dict,
    "values",
    [Typval::dict(vec![
        (OxStr::from("a"), number(1)),
        (OxStr::from("b"), number(2))
    ])],
    list(&[1, 2])
);
case!(
    items_dict,
    "items",
    [Typval::dict(vec![(OxStr::from("a"), number(1))])],
    Typval::list(vec![Typval::list(vec![text("a"), number(1)])])
);
case!(max_list, "max", [list(&[1, 9, 2])], number(9));
case!(min_list, "min", [list(&[1, -2, 9])], number(-2));
case!(max_empty, "max", [list(&[])], number(0));
case!(range_single, "range", [number(4)], list(&[0, 1, 2, 3]));
case!(
    range_bounds,
    "range",
    [number(2), number(4)],
    list(&[2, 3, 4])
);
case!(
    range_stride,
    "range",
    [number(2), number(8), number(3)],
    list(&[2, 5, 8])
);
case!(
    range_negative_stride,
    "range",
    [number(3), number(1), number(-1)],
    list(&[3, 2, 1])
);
case!(
    remove_list_item,
    "remove",
    [list(&[1, 2, 3]), number(1)],
    number(2)
);
case!(
    remove_list_range,
    "remove",
    [list(&[1, 2, 3, 4]), number(1), number(2)],
    list(&[2, 3])
);
case!(
    remove_dict_item,
    "remove",
    [Typval::dict(vec![(OxStr::from("a"), number(3))]), text("a")],
    number(3)
);
case!(
    sort_numbers,
    "sort",
    [list(&[3, 1, 2]), text("n")],
    list(&[1, 2, 3])
);
case!(
    sort_stable_equal,
    "sort",
    [list(&[2, 1, 2]), text("n")],
    list(&[1, 2, 2])
);
case!(
    uniq_adjacent,
    "uniq",
    [list(&[1, 1, 2, 1])],
    list(&[1, 2, 1])
);
case!(
    extend_lists,
    "extend",
    [list(&[1]), list(&[2, 3])],
    list(&[1, 2, 3])
);
case!(
    flatten_one,
    "flatten",
    [Typval::list(vec![number(1), list(&[2, 3])])],
    list(&[1, 2, 3])
);
case!(
    flatten_depth_zero,
    "flatten",
    [Typval::list(vec![list(&[1])]), number(0)],
    Typval::list(vec![list(&[1])])
);
case!(
    blob2list_basic,
    "blob2list",
    [Typval::Blob(vec![0, 255])],
    list(&[0, 255])
);
case!(
    list2blob_basic,
    "list2blob",
    [list(&[0, 255])],
    Typval::Blob(vec![0, 255])
);
case!(list2str_bytes, "list2str", [list(&[65, 66])], text("AB"));
case!(str2list_bytes, "str2list", [text("AB")], list(&[65, 66]));
case!(char2nr_ascii, "char2nr", [text("A")], number(65));
case!(char2nr_empty, "char2nr", [text("")], number(0));
case!(nr2char_ascii, "nr2char", [number(65)], text("A"));
case!(nr2char_unicode, "nr2char", [number(0xE9)], text("é"));
case!(str2nr_decimal, "str2nr", [text("123")], number(123));
case!(str2nr_negative, "str2nr", [text("-42")], number(-42));
case!(
    str2nr_hex_explicit,
    "str2nr",
    [text("0xff"), number(16)],
    number(255)
);
case!(
    str2nr_binary_explicit,
    "str2nr",
    [text("0b101"), number(2)],
    number(5)
);
case!(
    str2nr_octal_explicit,
    "str2nr",
    [text("0o17"), number(8)],
    number(15)
);
case!(
    str2nr_default_is_decimal,
    "str2nr",
    [text("0xff")],
    number(0)
);
case!(
    str2float_basic,
    "str2float",
    [text("1.25")],
    Typval::Float(1.25)
);
// `eval/typval_defs.h:123-133`, each measured on the oracle.
case!(type_number, "type", [number(1)], number(0));
case!(type_string, "type", [text("x")], number(1));
case!(type_func, "type", [funcref("tr")], number(2));
case!(type_list, "type", [list(&[])], number(3));
case!(type_dict, "type", [Typval::dict(vec![])], number(4));
case!(type_float, "type", [Typval::Float(1.0)], number(5));
case!(type_bool, "type", [Typval::Bool(true)], number(6));
case!(
    type_null,
    "type",
    [Typval::Special(Special::Null)],
    number(7)
);
case!(type_blob, "type", [Typval::Blob(vec![0])], number(10));
case!(string_number, "string", [number(12)], text("12"));
case!(string_text_quotes, "string", [text("a")], text("'a'"));
case!(string_bool, "string", [Typval::Bool(true)], text("v:true"));
case!(
    json_encode_null,
    "json_encode",
    [Typval::Special(Special::Null)],
    text("null")
);
case!(
    json_encode_bool,
    "json_encode",
    [Typval::Bool(true)],
    text("true")
);
case!(
    json_encode_list,
    "json_encode",
    [list(&[1, 2])],
    text("[1,2]")
);
case!(
    json_encode_dict_order,
    "json_encode",
    [Typval::dict(vec![
        (OxStr::from("b"), number(2)),
        (OxStr::from("a"), number(1))
    ])],
    text("{\"b\":2,\"a\":1}")
);
case!(
    json_decode_null,
    "json_decode",
    [text("null")],
    Typval::Special(Special::Null)
);
case!(
    json_decode_bool,
    "json_decode",
    [text("false")],
    Typval::Bool(false)
);
case!(
    json_decode_list,
    "json_decode",
    [text("[1,2]")],
    list(&[1, 2])
);

const FROZEN_INVENTORY: &[&str] = &[
    "abs",
    "acos",
    "add",
    "and",
    "api_info",
    "append",
    "appendbufline",
    "argc",
    "argidx",
    "arglistid",
    "argv",
    "asin",
    "assert_beeps",
    "assert_equal",
    "assert_equalfile",
    "assert_exception",
    "assert_fails",
    "assert_false",
    "assert_inrange",
    "assert_match",
    "assert_nobeep",
    "assert_notequal",
    "assert_notmatch",
    "assert_report",
    "assert_true",
    "atan",
    "atan2",
    "blob2list",
    "browse",
    "browsedir",
    "bufadd",
    "bufexists",
    "buffer_exists",
    "buffer_name",
    "buffer_number",
    "buflisted",
    "bufload",
    "bufloaded",
    "bufname",
    "bufnr",
    "bufwinid",
    "bufwinnr",
    "byte2line",
    "byteidx",
    "byteidxcomp",
    "call",
    "ceil",
    "chanclose",
    "changenr",
    "chansend",
    "char2nr",
    "charclass",
    "charcol",
    "charidx",
    "chdir",
    "cindent",
    "clearmatches",
    "cmdcomplete_info",
    "col",
    "complete",
    "complete_add",
    "complete_check",
    "complete_info",
    "confirm",
    "copy",
    "cos",
    "cosh",
    "count",
    "cursor",
    "debugbreak",
    "deepcopy",
    "delete",
    "deletebufline",
    "dictwatcheradd",
    "dictwatcherdel",
    "did_filetype",
    "diff_filler",
    "diff_hlID",
    "digraph_get",
    "digraph_getlist",
    "digraph_set",
    "digraph_setlist",
    "empty",
    "environ",
    "escape",
    "eval",
    "eventhandler",
    "executable",
    "execute",
    "exepath",
    "exists",
    "exp",
    "expand",
    "expandcmd",
    "extend",
    "extendnew",
    "feedkeys",
    "file_readable",
    "filecopy",
    "filereadable",
    "filewritable",
    "filter",
    "finddir",
    "findfile",
    "flatten",
    "flattennew",
    "float2nr",
    "floor",
    "fmod",
    "fnameescape",
    "fnamemodify",
    "foldclosed",
    "foldclosedend",
    "foldlevel",
    "foldtext",
    "foldtextresult",
    "foreach",
    "foreground",
    "fullcommand",
    "funcref",
    "function",
    "garbagecollect",
    "get",
    "getbufinfo",
    "getbufline",
    "getbufoneline",
    "getbufvar",
    "getcellwidths",
    "getchangelist",
    "getchar",
    "getcharmod",
    "getcharpos",
    "getcharsearch",
    "getcharstr",
    "getcmdcomplpat",
    "getcmdcompltype",
    "getcmdline",
    "getcmdpos",
    "getcmdprompt",
    "getcmdscreenpos",
    "getcmdtype",
    "getcmdwintype",
    "getcompletion",
    "getcompletiontype",
    "getcurpos",
    "getcursorcharpos",
    "getcwd",
    "getenv",
    "getfontname",
    "getfperm",
    "getfsize",
    "getftime",
    "getftype",
    "getjumplist",
    "getline",
    "getloclist",
    "getmarklist",
    "getmatches",
    "getmousepos",
    "getpid",
    "getpos",
    "getqflist",
    "getreg",
    "getreginfo",
    "getregion",
    "getregionpos",
    "getregtype",
    "getscriptinfo",
    "getstacktrace",
    "gettabinfo",
    "gettabvar",
    "gettabwinvar",
    "gettagstack",
    "gettext",
    "getwininfo",
    "getwinpos",
    "getwinposx",
    "getwinposy",
    "getwinvar",
    "glob",
    "glob2regpat",
    "globpath",
    "has",
    "has_key",
    "haslocaldir",
    "hasmapto",
    "highlightID",
    "highlight_exists",
    "histadd",
    "histdel",
    "histget",
    "histnr",
    "hlID",
    "hlexists",
    "hostname",
    "iconv",
    "id",
    "indent",
    "index",
    "indexof",
    "input",
    "inputdialog",
    "inputlist",
    "inputrestore",
    "inputsave",
    "inputsecret",
    "insert",
    "interrupt",
    "invert",
    "isabsolutepath",
    "isdirectory",
    "isinf",
    "islocked",
    "isnan",
    "items",
    "jobclose",
    "jobpid",
    "jobresize",
    "jobsend",
    "jobstart",
    "jobstop",
    "jobwait",
    "join",
    "json_decode",
    "json_encode",
    "keys",
    "keytrans",
    "last_buffer_nr",
    "len",
    "libcall",
    "libcallnr",
    "line",
    "line2byte",
    "lispindent",
    "list2blob",
    "list2str",
    "localtime",
    "log",
    "log10",
    "luaeval",
    "map",
    "maparg",
    "mapcheck",
    "maplist",
    "mapnew",
    "mapset",
    "match",
    "matchadd",
    "matchaddpos",
    "matcharg",
    "matchbufline",
    "matchdelete",
    "matchend",
    "matchfuzzy",
    "matchfuzzypos",
    "matchlist",
    "matchstr",
    "matchstrlist",
    "matchstrpos",
    "max",
    "menu_get",
    "menu_info",
    "min",
    "mkdir",
    "mode",
    "msgpackdump",
    "msgpackparse",
    "nextnonblank",
    "nr2char",
    "nvim_...",
    "or",
    "pathshorten",
    "perleval",
    "pow",
    "preinserted",
    "prevnonblank",
    "printf",
    "prompt_appendbuf",
    "prompt_getinput",
    "prompt_getprompt",
    "prompt_setcallback",
    "prompt_setinterrupt",
    "prompt_setprompt",
    "pum_getpos",
    "pumvisible",
    "py3eval",
    "pyeval",
    "pyxeval",
    "rand",
    "range",
    "readblob",
    "readdir",
    "readfile",
    "reduce",
    "reg_executing",
    "reg_recorded",
    "reg_recording",
    "reltime",
    "reltimefloat",
    "reltimestr",
    "remove",
    "rename",
    "repeat",
    "resolve",
    "reverse",
    "round",
    "rpcnotify",
    "rpcrequest",
    "rpcstart",
    "rpcstop",
    "rubyeval",
    "screenattr",
    "screenchar",
    "screenchars",
    "screencol",
    "screenpos",
    "screenrow",
    "screenstring",
    "search",
    "searchcount",
    "searchdecl",
    "searchpair",
    "searchpairpos",
    "searchpos",
    "serverlist",
    "serverstart",
    "serverstop",
    "setbufline",
    "setbufvar",
    "setcellwidths",
    "setcharpos",
    "setcharsearch",
    "setcmdline",
    "setcmdpos",
    "setcursorcharpos",
    "setenv",
    "setfperm",
    "setline",
    "setloclist",
    "setmatches",
    "setpos",
    "setqflist",
    "setreg",
    "settabvar",
    "settabwinvar",
    "settagstack",
    "setwinvar",
    "sha256",
    "shellescape",
    "shiftwidth",
    "sign_define",
    "sign_getdefined",
    "sign_getplaced",
    "sign_jump",
    "sign_place",
    "sign_placelist",
    "sign_undefine",
    "sign_unplace",
    "sign_unplacelist",
    "simplify",
    "sin",
    "sinh",
    "slice",
    "sockconnect",
    "sort",
    "soundfold",
    "spellbadword",
    "spellsuggest",
    "split",
    "sqrt",
    "srand",
    "state",
    "stdioopen",
    "stdpath",
    "str2float",
    "str2list",
    "str2nr",
    "strcharlen",
    "strcharpart",
    "strchars",
    "strdisplaywidth",
    "strftime",
    "strgetchar",
    "stridx",
    "string",
    "strlen",
    "strpart",
    "strptime",
    "strridx",
    "strtrans",
    "strutf16len",
    "strwidth",
    "submatch",
    "substitute",
    "swapfilelist",
    "swapinfo",
    "swapname",
    "synID",
    "synIDattr",
    "synIDtrans",
    "synconcealed",
    "synstack",
    "system",
    "systemlist",
    "tabpagebuflist",
    "tabpagenr",
    "tabpagewinnr",
    "tagfiles",
    "taglist",
    "tan",
    "tanh",
    "tempname",
    "termopen",
    "test_garbagecollect_now",
    "test_write_list_log",
    "timer_info",
    "timer_pause",
    "timer_start",
    "timer_stop",
    "timer_stopall",
    "tolower",
    "toupper",
    "tr",
    "trim",
    "trunc",
    "type",
    "undofile",
    "undotree",
    "uniq",
    "utf16idx",
    "values",
    "virtcol",
    "virtcol2col",
    "visualmode",
    "wait",
    "wildmenumode",
    "wildtrigger",
    "win_execute",
    "win_findbuf",
    "win_getid",
    "win_gettype",
    "win_gotoid",
    "win_id2tabwin",
    "win_id2win",
    "win_move_separator",
    "win_move_statusline",
    "win_screenpos",
    "win_splitmove",
    "winbufnr",
    "wincol",
    "windowsversion",
    "winheight",
    "winlayout",
    "winline",
    "winnr",
    "winrestcmd",
    "winrestview",
    "winsaveview",
    "winwidth",
    "wordcount",
    "writefile",
    "xor",
];

#[test]
fn builtin_inventory_manifest_is_frozen() {
    // Audit (`.outline/evidence/eval-inventory-audit.md`): the test-side
    // `eval.lua` clone could only guard completeness by re-parsing the
    // corpus, which agrees with the generator on every parse quirk. The
    // corpus under `codegen/upstream/` is a pinned copy, so freezing its
    // folded 462-name manifest pins completeness (a), uniqueness (b), sort
    // order (c), and name-over-key folding (g) without a second parser.
    // A corpus bump or generator regression changes this list on purpose.

    assert_eq!(BUILTINS.len(), FROZEN_INVENTORY.len());
    for (spec, frozen) in BUILTINS.iter().zip(FROZEN_INVENTORY) {
        assert_eq!(spec.name, *frozen);
    }
}

#[test]
fn builtin_specs_pin_distinctive_generated_fields() {
    // Direct pins for the field classes the clone test guarded, chosen so
    // each row fails for a different generator regression: overload names
    // fold (`cursor` absorbs `cursor__1`, whose key must NOT survive),
    // arity widens across overloads (max unbounded for `expand`/`getreg`,
    // min lowered for `getreg`/`reltime`, four-way fold for `remove`),
    // an explicit `name` beats the table key (`nvim_api__` -> `nvim_...`),
    // and non-method builtins still carry ranged or unbounded arity.
    for (name, min_args, max_args, method) in [
        ("cursor", 1, Some(3), true),
        ("expand", 1, None, true),
        ("getreg", 0, None, true),
        ("remove", 2, Some(3), true),
        ("reltime", 0, Some(2), true),
        ("nvim_...", 1, Some(1), true),
        ("wait", 2, Some(3), false),
        ("rpcnotify", 2, None, false),
    ] {
        let Some(spec) = crate::builtins::builtin_spec(name) else {
            panic!("{name} missing from the generated inventory");
        };
        assert_eq!(
            (spec.min_args, spec.max_args, spec.method),
            (min_args, max_args, method),
            "{name}"
        );
    }
    // The overload suffix key must fold away: it never exists as an entry.
    assert!(crate::builtins::builtin_spec("cursor__1").is_none());
    assert!(crate::builtins::builtin_spec("remove__4").is_none());
}

#[test]
fn key_only_entries_carry_generated_arity() {
    // `eval.lua` declares these without a `name` field, so the generator
    // must fall back to the Lua table key; they were silently dropped
    // before the fallback. `and` pins the quoted `['…']` key form and
    // proves an explicit `name` still wins over the key. Inventory
    // membership is metadata only: dispatch stays gated by
    // `is_builtin_implemented`, so nothing here claims an implementation
    // (notably `test_garbagecollect_now`).
    let cases: [(&str, usize, Option<usize>, bool); 11] = [
        ("and", 2, Some(2), true),
        ("foreground", 0, Some(0), false),
        ("highlightID", 1, Some(1), true),
        ("highlight_exists", 1, Some(1), true),
        ("inputdialog", 1, Some(3), true),
        ("jobclose", 1, Some(2), false),
        ("jobsend", 2, Some(2), false),
        ("last_buffer_nr", 0, Some(0), false),
        ("rpcstop", 1, Some(1), false),
        ("test_garbagecollect_now", 0, Some(0), false),
        ("test_write_list_log", 1, Some(1), false),
    ];
    for (name, min_args, max_args, method) in cases {
        let Some(spec) = crate::builtins::builtin_spec(name) else {
            panic!("{name} dropped from the generated inventory");
        };
        assert_eq!(
            (spec.min_args, spec.max_args, spec.method),
            (min_args, max_args, method),
            "{name}"
        );
    }
}

#[test]
fn method_base_matches_upstream_and_drives_receiver_slot() {
    // `method_base` is the only generated field with a live consumer outside
    // metadata: `eval.rs` inserts the `->` receiver at `method_base - 1`.
    // The audit (`.outline/evidence/eval-inventory-audit.md` class e) found
    // the `method` flag has clone coverage but `method_base` has none — a
    // regressed `base` parse would silently collapse every non-1 receiver.
    // Cases pin each distinct upstream base value (0 = not a method, 1, 2,
    // 3, 4) plus the name-over-key precedence for a base-carrying entry.
    for (name, method_base) in [
        ("foreground", 0),
        ("and", 1),
        ("printf", 2),
        ("appendbufline", 3),
        ("settabwinvar", 4),
    ] {
        let Some(spec) = crate::builtins::builtin_spec(name) else {
            panic!("{name} dropped from the generated inventory");
        };
        assert_eq!(spec.method_base, method_base, "{name}");
        assert_eq!(spec.method, method_base > 0, "{name}");
    }
    // Name-over-key precedence for a base-carrying entry: `cursor = { name =
    // 'cursor', base = 1 }` must resolve to base 1, not to a `cursor__1`-style
    // key fallback (audit class g).
    let Some(cursor) = crate::builtins::builtin_spec("cursor") else {
        panic!("cursor dropped from the generated inventory");
    };
    assert_eq!(cursor.method_base, 1, "cursor");
    assert!(crate::builtins::builtin_spec("cursor__1").is_none());

    // Behavior: the receiver lands at the base slot. `printf` has base 2,
    // so `42->printf("fmt=%d")` calls printf("fmt=%d", 42) — the receiver
    // goes second, not first. A regressed base (1) would call
    // printf(42, "fmt=%d") and fail on a numeric format.
    // (`eval.lua:28-30`: the `LAST` base exists upstream but is unused since
    // v8.2.1168 — no corpus entry carries it, so only integer bases 1-4 are
    // pinned here. `call_method` in `builtins.rs` rejects base-0 builtins
    // with E276 before the slot math runs, so `map_or(1, ...)` in `eval.rs`
    // is unreachable for non-methods.)
    let mut builtins = Builtins::without_regex();
    let mut scope = Scope::new();
    let mut run = |source: &[u8]| {
        let program = Parser::new(source).parse().unwrap();
        Evaluator::new(&mut builtins, &NoRegex)
            .eval(&program, &mut scope)
            .unwrap()
    };
    assert_eq!(run(b"42->printf(\"fmt=%d\")"), text("fmt=42"));
    // Base 1: the receiver goes first. `abs` has base 1.
    assert_eq!(run(b"-5->abs()"), number(5));
    // Base 0: not a method — E276, matching `call_method`'s guard (and
    // upstream `FCERR_NOTMETHOD` at `funcs.c:304`).
    let program = Parser::new(b"1->foreground()").parse().unwrap();
    let error = Evaluator::new(&mut builtins, &NoRegex)
        .eval(&program, &mut scope)
        .unwrap_err();
    assert_eq!(error.code, "E276");
}

#[test]
fn arity_errors_are_vim_compatible() {
    assert_eq!(call("abs", vec![]).unwrap_err().code, "E119");
    assert_eq!(
        call("abs", vec![number(1), number(2)]).unwrap_err().code,
        "E118"
    );
}

#[test]
fn non_pure_builtin_is_typed_not_implemented() {
    let error = call("append", vec![number(1), text("x")]).unwrap_err();
    assert_eq!(
        error.kind,
        EvalErrorKind::NotImplemented(OxStr::from("append"))
    );
    let wrong_arity = call("api_info", vec![number(1)]).unwrap_err();
    assert_eq!(
        wrong_arity.kind,
        EvalErrorKind::NotImplemented(OxStr::from("api_info"))
    );
}

#[test]
fn unknown_builtin_is_typed_not_implemented() {
    let error = call("definitely_missing", vec![]).unwrap_err();
    assert_eq!(
        error.kind,
        EvalErrorKind::NotImplemented(OxStr::from("definitely_missing"))
    );
}

#[test]
fn method_call_injects_receiver_for_flagged_builtin() {
    // `runtime/doc/builtin.txt: add()` method form.
    let expression = Parser::new(b"[1, 2]->add(3)").parse().unwrap();
    let regex = NoRegex;
    let mut builtins = Builtins::without_regex();
    let result = Evaluator::new(&mut builtins, &regex)
        .eval(&expression, &mut Scope::new())
        .unwrap();
    assert_eq!(result, list(&[1, 2, 3]));
}

#[test]
fn chained_method_calls_preserve_receiver_order() {
    let expression = Parser::new(b"[1]->add(2)->add(3)").parse().unwrap();
    let regex = NoRegex;
    let mut builtins = Builtins::without_regex();
    let result = Evaluator::new(&mut builtins, &regex)
        .eval(&expression, &mut Scope::new())
        .unwrap();
    assert_eq!(result, list(&[1, 2, 3]));
}

#[test]
fn map_string_callback_uses_v_val_and_v_key() {
    // `test/old/testdir/test_listdict.vim`: expression callback form.
    let result = call("map", vec![list(&[2, 3]), text("v:val * 2 + v:key")]).unwrap();
    assert_eq!(result, list(&[4, 7]));
}

#[test]
fn filter_string_callback_uses_v_val() {
    let result = call("filter", vec![list(&[0, 2, 0, 3]), text("v:val")]).unwrap();
    assert_eq!(result, list(&[2, 3]));
}

#[test]
fn nr2char_accepts_legacy_six_byte_encoding() {
    assert_eq!(
        call("nr2char", vec![number(0x4000_0000)]).unwrap(),
        Typval::String(OxStr(vec![0xfd, 0x80, 0x80, 0x80, 0x80, 0x80]))
    );
}

#[test]
fn range_zero_stride_has_error_code() {
    assert_eq!(
        call("range", vec![number(1), number(3), number(0)])
            .unwrap_err()
            .code,
        "E726"
    );
}

#[test]
fn list_as_number_has_error_code() {
    assert_eq!(call("abs", vec![list(&[])]).unwrap_err().code, "E745");
}

#[test]
fn sort_default_comparator_uses_strings() {
    // `test/old/testdir/test_functions.vim` `Test_sort_numbers()` and
    // the default comparator in `item_compare` (typval.c:1192-1310): values
    // are converted to strings, so `sort([2, 10])` is `[10, 2]`.
    let result = call("sort", vec![list(&[2, 10])]).unwrap();
    assert_eq!(result, list(&[10, 2]));
    // A String compared against a non-String sorts as a leading quote.
    let mixed = call("sort", vec![Typval::list(vec![number(0), text("x")])]).unwrap();
    assert_eq!(mixed, Typval::list(vec![text("x"), number(0)]));
}

#[test]
fn sort_numeric_mode() {
    // `n` mode: each value is stringified and its leading number parsed, so
    // numeric values order numerically while strings order as 0.
    let result = call("sort", vec![list(&[2, 10, 1]), text("n")]).unwrap();
    assert_eq!(result, list(&[1, 2, 10]));
    let mixed = call(
        "sort",
        vec![
            Typval::list(vec![text("a"), number(5), number(3)]),
            text("n"),
        ],
    )
    .unwrap();
    assert_eq!(mixed, Typval::list(vec![text("a"), number(3), number(5)]));
}

#[test]
fn sort_integer_mode() {
    // `N` mode: integer comparison via `tv_get_number`.
    let result = call("sort", vec![list(&[10, 2, -1]), text("N")]).unwrap();
    assert_eq!(result, list(&[-1, 2, 10]));
}

#[test]
fn sort_float_mode() {
    // `f` mode: float comparison via `tv_get_float`.
    let values = Typval::list(vec![
        Typval::Float(2.5),
        Typval::Float(1.5),
        Typval::Float(10.0),
    ]);
    let result = call("sort", vec![values, text("f")]).unwrap();
    assert_eq!(
        result,
        Typval::list(vec![
            Typval::Float(1.5),
            Typval::Float(2.5),
            Typval::Float(10.0)
        ])
    );
}

#[test]
fn sort_ignore_case_mode() {
    // `i` mode: case-insensitive string sort.
    let result = call(
        "sort",
        vec![
            Typval::list(vec![text("banana"), text("Apple"), text("cherry")]),
            text("i"),
        ],
    )
    .unwrap();
    assert_eq!(
        result,
        Typval::list(vec![text("Apple"), text("banana"), text("cherry")])
    );
}

#[test]
fn sort_locale_mode_is_byte_wise_fallback() {
    // `l` mode is documented to sort by the locale of the running system; this
    // port uses a byte-wise fallback for the C-locale `strcoll` comparison, so
    // it matches the default ordering here.
    let result = call(
        "sort",
        vec![Typval::list(vec![text("banana"), text("Apple")]), text("l")],
    )
    .unwrap();
    assert_eq!(result, Typval::list(vec![text("Apple"), text("banana")]));
    let numbers = call("sort", vec![list(&[2, 10]), text("l")]).unwrap();
    assert_eq!(numbers, list(&[10, 2]));
}

#[test]
fn sort_callback_stops_after_first_failure_and_retains_first_error() {
    // Once the comparator errors, `sort()` must not invoke it again for later
    // pairs, and the original error must be returned rather than overwritten.
    let mut scope = Scope::new();
    let counter = Typval::list(vec![]);
    scope.set(b"counter", counter.clone()).unwrap();
    let values = Typval::list(vec![number(3), number(1), number(2)]);
    let callback = text("add(counter, 1) + missing");
    let mut builtins = Builtins::without_regex();
    let error = builtins
        .call(&OxStr::from("sort"), vec![values, callback], &mut scope)
        .unwrap_err();
    assert_eq!(error.code, "E121");
    assert_eq!(counter, Typval::list(vec![number(1)]));
}

#[test]
fn str2nr_base_zero_is_rejected() {
    // `test/old/testdir/test_functions.vim`: only 2/8/10/16 are valid bases;
    // base 0 is rejected with E474 (f_str2nr, strings.c:2593-2598).
    let error = call("str2nr", vec![text("0xff"), number(0)]).unwrap_err();
    assert_eq!(error.code, "E474");
    let error = call("str2nr", vec![text("123"), number(1)]).unwrap_err();
    assert_eq!(error.code, "E474");
}

#[test]
fn str2nr_allows_whitespace_after_sign() {
    // `test/old/testdir/test_functions.vim` `Test_str2nr()`: whitespace after
    // the sign is skipped.
    assert_eq!(call("str2nr", vec![text("+ 1")]).unwrap(), number(1));
    assert_eq!(call("str2nr", vec![text("- 1")]).unwrap(), number(-1));
    assert_eq!(call("str2nr", vec![text(" - 42 ")]).unwrap(), number(-42));
    assert_eq!(
        call("str2nr", vec![text("+ 10"), number(16)]).unwrap(),
        number(16)
    );
}

#[test]
fn str2nr_prefix_rules_follow_force_mode() {
    // `test/old/testdir/test_functions.vim` `Test_str2nr()`: with an explicit
    // base the "0b"/"0o"/"0x" prefix is consumed (STR2NR_FORCE), and text
    // after the parsed digits is ignored.
    assert_eq!(
        call("str2nr", vec![text("0101"), number(8)]).unwrap(),
        number(65)
    );
    assert_eq!(
        call("str2nr", vec![text("0o0101"), number(8)]).unwrap(),
        number(65)
    );
    assert_eq!(
        call("str2nr", vec![text("-0b101"), number(2)]).unwrap(),
        number(-5)
    );
    assert_eq!(
        call("str2nr", vec![text("0Xabcdef"), number(16)]).unwrap(),
        number(11_259_375)
    );
    assert_eq!(
        call("str2nr", vec![text("12"), number(2)]).unwrap(),
        number(1)
    );
    assert_eq!(
        call("str2nr", vec![text("18"), number(8)]).unwrap(),
        number(1)
    );
    assert_eq!(
        call("str2nr", vec![text("1g"), number(16)]).unwrap(),
        number(1)
    );
}

#[test]
fn nested_comparison_propagates_recursion_error() {
    // `test/old/testdir/test_listdict.vim`: recursive compare guard.
    let mut value = number(1);
    for _ in 0..101 {
        value = Typval::list(vec![value]);
    }
    assert_eq!(
        call("count", vec![Typval::list(vec![value.clone()]), value])
            .unwrap_err()
            .code,
        "E724"
    );
}

struct LiteralRegex {
    calls: Cell<usize>,
}
impl RegexEngine for LiteralRegex {
    fn is_match(&self, text: &OxStr, pattern: &OxStr, _ignore_case: bool) -> crate::Result<bool> {
        self.calls.set(self.calls.get() + 1);
        Ok(text
            .as_bytes()
            .windows(pattern.as_bytes().len())
            .any(|window| window == pattern.as_bytes()))
    }
    fn split(&self, text: &OxStr, pattern: &OxStr, keep_empty: bool) -> crate::Result<Vec<OxStr>> {
        self.calls.set(self.calls.get() + 1);
        let source = text.to_string_lossy();
        let pattern = pattern.to_string_lossy();
        Ok(source
            .split(pattern.as_ref())
            .filter(|part| keep_empty || !part.is_empty())
            .map(OxStr::from)
            .collect())
    }
    fn find(
        &self,
        text: &OxStr,
        pattern: &OxStr,
        start: usize,
    ) -> crate::Result<Option<(usize, usize)>> {
        self.calls.set(self.calls.get() + 1);
        Ok(text
            .as_bytes()
            .get(start..)
            .and_then(|tail| {
                tail.windows(pattern.as_bytes().len())
                    .position(|window| window == pattern.as_bytes())
            })
            .map(|position| {
                (
                    start + position,
                    start + position + pattern.as_bytes().len(),
                )
            }))
    }
    fn substitute(
        &self,
        text: &OxStr,
        pattern: &OxStr,
        replacement: &OxStr,
        flags: &OxStr,
    ) -> crate::Result<OxStr> {
        self.calls.set(self.calls.get() + 1);
        let source = text.to_string_lossy();
        let pattern = pattern.to_string_lossy();
        let replacement = replacement.to_string_lossy();
        let replaced = if flags.as_bytes().contains(&b'g') {
            source.replace(pattern.as_ref(), replacement.as_ref())
        } else {
            source.replacen(pattern.as_ref(), replacement.as_ref(), 1)
        };
        Ok(OxStr(replaced.into_bytes()))
    }
}

#[test]
fn split_uses_regex_engine_seam() {
    let regex = LiteralRegex {
        calls: Cell::new(0),
    };
    let mut builtins = Builtins::new(&regex);
    let result = builtins
        .call(
            &OxStr::from("split"),
            vec![text("a,b"), text(",")],
            &mut Scope::new(),
        )
        .unwrap();
    assert_eq!(result, Typval::list(vec![text("a"), text("b")]));
    assert_eq!(regex.calls.get(), 1);
}

#[test]
fn higher_order_callback_uses_supplied_regex_engine() {
    let regex = LiteralRegex {
        calls: Cell::new(0),
    };
    let mut builtins = Builtins::new(&regex);
    let result = builtins
        .call(
            &OxStr::from("filter"),
            vec![
                Typval::list(vec![text("alpha"), text("beta")]),
                text("v:val =~# 'ph'"),
            ],
            &mut Scope::new(),
        )
        .unwrap();
    assert_eq!(result, Typval::list(vec![text("alpha")]));
    assert_eq!(regex.calls.get(), 2);
}

#[test]
fn split_without_pattern_uses_whitespace_runs() {
    let regex = LiteralRegex {
        calls: Cell::new(0),
    };
    let mut builtins = Builtins::new(&regex);
    let result = builtins
        .call(
            &OxStr::from("split"),
            vec![text("  alpha\nbeta\t gamma  ")],
            &mut Scope::new(),
        )
        .unwrap();
    assert_eq!(
        result,
        Typval::list(vec![text("alpha"), text("beta"), text("gamma")]),
    );
    assert_eq!(regex.calls.get(), 0);
}

#[test]
fn match_family_uses_regex_engine_seam() {
    let regex = LiteralRegex {
        calls: Cell::new(0),
    };
    let mut builtins = Builtins::new(&regex);
    assert_eq!(
        builtins
            .call(
                &OxStr::from("match"),
                vec![text("abc"), text("b")],
                &mut Scope::new()
            )
            .unwrap(),
        number(1)
    );
    assert_eq!(
        builtins
            .call(
                &OxStr::from("matchend"),
                vec![text("abc"), text("b")],
                &mut Scope::new()
            )
            .unwrap(),
        number(2)
    );
    assert_eq!(
        builtins
            .call(
                &OxStr::from("matchstr"),
                vec![text("abc"), text("b")],
                &mut Scope::new()
            )
            .unwrap(),
        text("b")
    );
    assert_eq!(regex.calls.get(), 3);
}

#[test]
fn match_family_honors_count_and_list_inputs() {
    let regex = LiteralRegex {
        calls: Cell::new(0),
    };
    let mut builtins = Builtins::new(&regex);
    assert_eq!(
        builtins
            .call(
                &OxStr::from("match"),
                vec![text("ababa"), text("ba"), number(0), number(2)],
                &mut Scope::new()
            )
            .unwrap(),
        number(3)
    );
    assert_eq!(
        builtins
            .call(
                &OxStr::from("match"),
                vec![
                    Typval::list(vec![text("x"), text("ab"), text("ab")]),
                    text("b"),
                    number(0),
                    number(2)
                ],
                &mut Scope::new()
            )
            .unwrap(),
        number(2)
    );
}

#[test]
fn substitute_uses_regex_engine_seam() {
    let regex = LiteralRegex {
        calls: Cell::new(0),
    };
    let mut builtins = Builtins::new(&regex);
    let result = builtins
        .call(
            &OxStr::from("substitute"),
            vec![text("aba"), text("b"), text("x"), text("")],
            &mut Scope::new(),
        )
        .unwrap();
    assert_eq!(result, text("axa"));
    assert_eq!(regex.calls.get(), 1);
}

#[test]
fn regex_builtin_without_engine_is_typed_error() {
    assert_eq!(
        call("split", vec![text("a b"), text(" ")])
            .unwrap_err()
            .code,
        "E54"
    );
}

fn eval_builtin(source: &[u8], mut scope: Scope) -> (Typval, Scope) {
    let expression = Parser::new(source).parse().unwrap();
    let regex = NoRegex;
    let mut builtins = Builtins::without_regex();
    let result = Evaluator::new(&mut builtins, &regex)
        .eval(&expression, &mut scope)
        .unwrap();
    (result, scope)
}

/// `type(x) == v:t_<name>` is how plugin code actually spells a type check, so
/// the builtin and the variables have to agree value for value. Both sides are
/// pinned to the oracle: each expected number was measured by running
/// `type()` and `v:t_*` through `.references/neovim/build/bin/nvim`.
#[test]
fn type_matches_its_vim_type_variable_for_every_supported_type() {
    // `function()` is a host-layer builtin, so the Funcref case arrives as a
    // seeded variable rather than a literal call.
    let seeded = || {
        let mut scope = Scope::new();
        scope.set(b"Ref", funcref("tr")).unwrap();
        scope
    };
    let cases: [(&[u8], &[u8], i64); 9] = [
        (b"0", b"v:t_number", 0),
        (b"''", b"v:t_string", 1),
        (b"Ref", b"v:t_func", 2),
        (b"{x -> x}", b"v:t_func", 2),
        (b"[]", b"v:t_list", 3),
        (b"{}", b"v:t_dict", 4),
        (b"0.0", b"v:t_float", 5),
        (b"v:true", b"v:t_bool", 6),
        (b"0z00", b"v:t_blob", 10),
    ];
    for (value, variable, expected) in cases {
        let mut source = Vec::from(b"type(".as_slice());
        source.extend_from_slice(value);
        source.extend_from_slice(b") == ");
        source.extend_from_slice(variable);
        let name = String::from_utf8_lossy(variable).into_owned();
        assert_eq!(
            eval_builtin(&source, seeded()).0,
            number(1),
            "type({}) must equal {name}",
            String::from_utf8_lossy(value),
        );

        let mut probe = Vec::from(b"type(".as_slice());
        probe.extend_from_slice(value);
        probe.push(b')');
        assert_eq!(
            eval_builtin(&probe, seeded()).0,
            number(expected),
            "type({name}'s value)"
        );
        assert_eq!(
            eval_builtin(variable, seeded()).0,
            number(expected),
            "{name}"
        );
        assert_eq!(
            call("exists", vec![text(&name)]).unwrap(),
            number(1),
            "exists('{name}')"
        );
    }

    // `v:null` is `VAR_TYPE_SPECIAL`, which upstream gives no `v:t_` name:
    // `exists('v:t_special')` is 0 on the oracle.
    assert_eq!(eval_builtin(b"type(v:null)", Scope::new()).0, number(7));
    assert_eq!(
        call("exists", vec![text("v:t_special")]).unwrap(),
        number(0)
    );
}

#[test]
fn assignment_clone_shares_list_mutation() {
    let shared = list(&[1]);
    let mut scope = Scope::new();
    scope.set(b"a", shared.clone()).unwrap();
    scope.set(b"b", shared).unwrap();
    let (result, scope) = eval_builtin(b"add(a, 2)", scope);
    assert_eq!(result, list(&[1, 2]));
    assert_eq!(scope.get(b"b", 0).unwrap(), &list(&[1, 2]));
}

#[test]
fn identity_and_equality_distinguish_shared_lists() {
    let mut scope = Scope::new();
    let shared = list(&[1]);
    scope.set(b"a", shared.clone()).unwrap();
    scope.set(b"alias", shared).unwrap();
    scope.set(b"equal", list(&[1])).unwrap();
    assert_eq!(eval_builtin(b"a is alias", scope.clone()).0, number(1));
    assert_eq!(eval_builtin(b"a is equal", scope.clone()).0, number(0));
    assert_eq!(eval_builtin(b"a == equal", scope).0, number(1));
}

#[test]
fn copy_is_outer_independent_but_keeps_nested_aliases() {
    let nested = list(&[1]);
    let source = Typval::list(vec![nested.clone()]);
    let copied = call("copy", vec![source.clone()]).unwrap();
    call("add", vec![copied.clone(), number(9)]).unwrap();
    assert_eq!(call("len", vec![source.clone()]).unwrap(), number(1));
    let Typval::List(copied_ref) = copied else {
        panic!("List expected")
    };
    let copied_nested = copied_ref.borrow().items[0].clone();
    call("add", vec![copied_nested, number(2)]).unwrap();
    assert_eq!(nested, list(&[1, 2]));
}

#[test]
fn deepcopy_reproduces_cycles_and_breaks_source_aliases() {
    let source = Typval::list(vec![]);
    call("add", vec![source.clone(), source.clone()]).unwrap();
    assert_eq!(
        call("string", vec![source.clone()]).unwrap(),
        text("[[...]]")
    );
    let copied = call("deepcopy", vec![source.clone()]).unwrap();
    assert_eq!(
        call("string", vec![copied.clone()]).unwrap(),
        text("[[...]]")
    );
    let (Typval::List(source_ref), Typval::List(copy_ref)) = (&source, &copied) else {
        panic!("Lists expected")
    };
    assert!(!std::rc::Rc::ptr_eq(source_ref, copy_ref));
    let Typval::List(cycle_ref) = &copy_ref.borrow().items[0] else {
        panic!("cycle expected")
    };
    assert!(std::rc::Rc::ptr_eq(copy_ref, cycle_ref));
}

#[test]
fn cycle_equality_terminates_coinductively() {
    let left = Typval::list(vec![]);
    let right = Typval::list(vec![]);
    call("add", vec![left.clone(), left.clone()]).unwrap();
    call("add", vec![right.clone(), right.clone()]).unwrap();
    let mut scope = Scope::new();
    scope.set(b"left", left).unwrap();
    scope.set(b"right", right).unwrap();
    assert_eq!(eval_builtin(b"left == right", scope).0, number(1));
}

#[test]
fn shallow_and_deep_locks_enforce_mutation_and_report_state() {
    let nested = list(&[1]);
    let shallow = Typval::list(vec![nested.clone()]);
    crate::lock_value(&shallow, 1, true).unwrap();
    assert_eq!(crate::is_locked_value(&shallow).unwrap(), number(2));
    assert_eq!(
        call("add", vec![shallow, number(2)]).unwrap_err().code,
        "E741"
    );
    assert_eq!(crate::is_locked_value(&nested).unwrap(), number(0));

    let deep_nested = list(&[1]);
    let deep = Typval::list(vec![deep_nested.clone()]);
    crate::lock_value(&deep, -1, true).unwrap();
    assert_eq!(crate::is_locked_value(&deep).unwrap(), number(3));
    assert_eq!(crate::is_locked_value(&deep_nested).unwrap(), number(3));
    assert_eq!(
        call("add", vec![deep_nested, number(2)]).unwrap_err().code,
        "E741"
    );
}

#[test]
fn lambda_callbacks_cover_map_filter_sort_foreach_reduce() {
    assert_eq!(
        eval_builtin(b"map([1, 2, 3], {k, v -> v * 2})", Scope::new()).0,
        list(&[2, 4, 6])
    );
    assert_eq!(
        eval_builtin(b"filter([1, 2, 3, 4], {k, v -> v % 2})", Scope::new()).0,
        list(&[1, 3])
    );
    assert_eq!(
        eval_builtin(b"sort([3, 1, 2], {a, b -> a - b})", Scope::new()).0,
        list(&[1, 2, 3])
    );
    assert_eq!(
        eval_builtin(b"foreach([1, 2], {k, v -> v * 9})", Scope::new()).0,
        list(&[1, 2])
    );
    assert_eq!(
        eval_builtin(b"reduce([1, 2, 3], {a, v -> a + v})", Scope::new()).0,
        number(6)
    );
}

#[test]
fn mapnew_and_flattennew_do_not_mutate_inputs() {
    let source = list(&[1, 2]);
    let mut scope = Scope::new();
    scope.set(b"xs", source.clone()).unwrap();
    assert_eq!(
        eval_builtin(b"mapnew(xs, {k, v -> v + 10})", scope).0,
        list(&[11, 12])
    );
    assert_eq!(source, list(&[1, 2]));

    let nested = Typval::list(vec![number(1), list(&[2, 3])]);
    assert_eq!(
        call("flattennew", vec![nested.clone()]).unwrap(),
        list(&[1, 2, 3])
    );
    assert_eq!(nested, Typval::list(vec![number(1), list(&[2, 3])]));
}

#[test]
fn callback_structural_mutation_is_rejected_and_lock_restored() {
    let mut scope = Scope::new();
    scope.set(b"xs", list(&[1, 2])).unwrap();
    let expression = Parser::new(b"map(xs, {k, v -> add(xs, 9)})")
        .parse()
        .unwrap();
    let regex = NoRegex;
    let mut builtins = Builtins::without_regex();
    let error = Evaluator::new(&mut builtins, &regex)
        .eval(&expression, &mut scope)
        .unwrap_err();
    assert_eq!(error.code, "E741");
    let xs = scope.get(b"xs", 0).unwrap().clone();
    assert_eq!(call("add", vec![xs, number(3)]).unwrap(), list(&[1, 2, 3]));
}

#[test]
fn named_funcref_callbacks_cover_collection_builtins() {
    assert_eq!(
        call("map", vec![list(&[3, 3]), funcref("and")]).unwrap(),
        list(&[0, 1])
    );
    assert_eq!(
        call("filter", vec![list(&[3, 3]), funcref("and")]).unwrap(),
        list(&[3])
    );
    assert_eq!(
        call("foreach", vec![list(&[3, 3]), funcref("and")]).unwrap(),
        list(&[3, 3])
    );
    assert_eq!(
        call("reduce", vec![list(&[1, 2, 3]), funcref("or")]).unwrap(),
        number(3)
    );
    let sorted = call("sort", vec![list(&[1, 2]), funcref("and")]).unwrap();
    assert_eq!(call("len", vec![sorted]).unwrap(), number(2));
}

#[test]
fn string_expression_callbacks_cover_collection_builtins() {
    assert_eq!(
        call("map", vec![list(&[1, 2]), text("v:val * 3")]).unwrap(),
        list(&[3, 6])
    );
    assert_eq!(
        call("filter", vec![list(&[1, 2, 3]), text("v:val % 2")]).unwrap(),
        list(&[1, 3])
    );
    assert_eq!(
        call("foreach", vec![list(&[1, 2]), text("v:val * 9")]).unwrap(),
        list(&[1, 2])
    );
    assert_eq!(
        call("reduce", vec![list(&[1, 2, 3]), text("v:key + v:val")]).unwrap(),
        number(6)
    );
    // sort() comparator form: `runtime/doc/builtin.txt` sort().
    assert_eq!(
        call("sort", vec![list(&[3, 1, 2]), text("v:key - v:val")]).unwrap(),
        list(&[1, 2, 3])
    );
}

#[test]
fn scope_lockvar_facade_reports_all_container_lock_states() {
    let direct = list(&[]);
    let Typval::List(reference) = &direct else {
        panic!("List expected")
    };
    reference.borrow_mut().lock.locked = true;
    let mut scope = Scope::new();
    scope.set(b"direct", direct).unwrap();
    scope.set(b"shallow", list(&[])).unwrap();
    scope.set(b"deep", list(&[])).unwrap();
    assert_eq!(scope.islocked(b"missing", 0).unwrap_err().code, "E121");
    assert_eq!(scope.islocked(b"direct", 0).unwrap(), 1);
    scope.lockvar(b"shallow", 1).unwrap();
    scope.lockvar(b"deep", -1).unwrap();
    assert_eq!(scope.islocked(b"shallow", 0).unwrap(), 2);
    assert_eq!(scope.islocked(b"deep", 0).unwrap(), 3);
}

#[test]
fn callback_collections_support_blob_and_string_inputs() {
    assert_eq!(
        call("map", vec![Typval::Blob(vec![1, 2]), text("v:val + 1")]).unwrap(),
        Typval::Blob(vec![2, 3])
    );
    assert_eq!(
        call("mapnew", vec![Typval::Blob(vec![1, 2]), text("v:val + 2")]).unwrap(),
        Typval::Blob(vec![3, 4])
    );
    assert_eq!(
        call(
            "filter",
            vec![Typval::Blob(vec![1, 2, 3]), text("v:val % 2")]
        )
        .unwrap(),
        Typval::Blob(vec![1, 3])
    );
    assert_eq!(
        call("foreach", vec![Typval::Blob(vec![1, 2]), text("v:val + 9")]).unwrap(),
        Typval::Blob(vec![1, 2])
    );

    assert_eq!(
        call("map", vec![text("ab"), text("'x'")]).unwrap(),
        text("xx")
    );
    assert_eq!(
        call("mapnew", vec![text("ab"), text("'y'")]).unwrap(),
        text("yy")
    );
    assert_eq!(
        call("filter", vec![text("abc"), text("v:key % 2")]).unwrap(),
        text("b")
    );
    assert_eq!(
        call("foreach", vec![text("ab"), text("v:key")]).unwrap(),
        text("ab")
    );
}

#[test]
fn reduce_supports_blob_and_string_inputs() {
    assert_eq!(
        call(
            "reduce",
            vec![Typval::Blob(vec![1, 2, 3]), text("v:key + v:val")]
        )
        .unwrap(),
        number(6)
    );
    assert_eq!(
        call("reduce", vec![text("abc"), text("v:key")]).unwrap(),
        text("a")
    );
}

#[test]
fn map_exposes_prior_mutations_and_keeps_them_after_later_error() {
    let shared = list(&[1, 2]);
    let mut scope = Scope::new();
    scope.set(b"xs", shared.clone()).unwrap();
    assert_eq!(
        eval_builtin(b"map(xs, {k, v -> k ? xs[0] : 9})", scope).0,
        list(&[9, 9])
    );

    let partial = list(&[1, 2]);
    let mut scope = Scope::new();
    scope.set(b"xs", partial.clone()).unwrap();
    let expression = Parser::new(b"map(xs, {k, v -> k ? missing : 9})")
        .parse()
        .unwrap();
    let regex = NoRegex;
    let mut builtins = Builtins::without_regex();
    assert_eq!(
        Evaluator::new(&mut builtins, &regex)
            .eval(&expression, &mut scope)
            .unwrap_err()
            .code,
        "E121"
    );
    assert_eq!(partial, list(&[9, 2]));
    assert_eq!(
        call("add", vec![partial, number(3)]).unwrap(),
        list(&[9, 2, 3])
    );
}

#[test]
fn string_callbacks_preserve_invalid_bytes() {
    let raw = Typval::String(OxStr(vec![0xff, b'a']));
    assert_eq!(call("map", vec![raw.clone(), text("v:val")]).unwrap(), raw);
    assert_eq!(call("filter", vec![raw.clone(), text("1")]).unwrap(), raw);
    assert_eq!(
        call("foreach", vec![raw.clone(), text("v:val")]).unwrap(),
        raw
    );
    assert_eq!(
        call("reduce", vec![raw, text("v:key")]).unwrap(),
        Typval::String(OxStr(vec![0xff]))
    );
}

// ── Buffer-seam builtins: getline / setline ───────────────────────────
// Upstream: src/nvim/eval/buffer.c set_buffer_lines / get_buffer_lines,
// f_setline / f_getline; runtime/doc/vimfn.txt setline() / getline();
// test/old/testdir/test_bufline.vim covers the same surface per-buffer.

/// Minimal seam double: a flat line list with 1-based addressing, exactly
/// the operations `BufferHost` promises.
#[derive(Default)]
struct FakeBuffer {
    lines: Vec<String>,
    cursor: Option<i64>,
    marks: Vec<(char, i64)>,
}

impl FakeBuffer {
    fn new(lines: &[&str]) -> Self {
        Self {
            lines: lines.iter().map(|line| (*line).to_owned()).collect(),
            cursor: None,
            marks: Vec::new(),
        }
    }
}

impl crate::eval::BufferHost for FakeBuffer {
    fn line_count(&self) -> crate::Result<usize> {
        Ok(self.lines.len())
    }

    fn get_line(&self, lnum: usize) -> crate::Result<Option<OxStr>> {
        Ok(self
            .lines
            .get(lnum - 1)
            .map(|line| OxStr::from(line.as_str())))
    }

    fn replace_line(&mut self, lnum: usize, text: &OxStr) -> crate::Result<()> {
        self.lines[lnum - 1] = text.to_string_lossy().into_owned();
        Ok(())
    }

    fn append_line(&mut self, text: &OxStr) -> crate::Result<()> {
        self.lines.push(text.to_string_lossy().into_owned());
        Ok(())
    }

    fn address_line(&self, address: &str) -> crate::Result<Option<i64>> {
        let mut chars = address.chars();
        match chars.next() {
            Some('.') if chars.next().is_none() => Ok(self.cursor),
            Some('\'') => Ok(chars.next().and_then(|name| {
                self.marks
                    .iter()
                    .find(|(mark, _)| *mark == name)
                    .map(|(_, line)| *line)
            })),
            _ => Ok(None),
        }
    }
}

fn buffer_call(lines: &[&str], name: &str, args: &[Typval]) -> (crate::Result<Typval>, FakeBuffer) {
    let mut buffer = FakeBuffer::new(lines);
    let result = crate::builtins::call_buffer_builtin(&mut buffer, name, args);
    (result, buffer)
}

fn texts(values: &[&str]) -> Typval {
    Typval::list(values.iter().map(|value| text(value)).collect())
}

#[test]
fn setline_replaces_existing_line_and_returns_zero() {
    // f_setline → set_buffer_lines: `lnum <= ml_line_count` replaces.
    let (result, buffer) = buffer_call(&["a", "b", "c"], "setline", &[number(2), text("x")]);
    assert_eq!(result.unwrap(), number(0));
    assert_eq!(buffer.lines, vec!["a", "x", "c"]);
}

#[test]
fn setline_appends_just_past_the_last_line() {
    // builtin.txt setline(): "When {lnum} is just below the last line the
    // {text} will be added below the last line."
    let (result, buffer) = buffer_call(&["a", "b", "c"], "setline", &[number(4), text("d")]);
    assert_eq!(result.unwrap(), number(0));
    assert_eq!(buffer.lines, vec!["a", "b", "c", "d"]);
}

#[test]
fn setline_beyond_line_count_plus_one_fails_without_writing() {
    // set_buffer_lines: `lnum > ml_line_count + 1` breaks with FAIL (1).
    let (result, buffer) = buffer_call(&["a", "b", "c"], "setline", &[number(5), text("x")]);
    assert_eq!(result.unwrap(), number(1));
    assert_eq!(buffer.lines, vec!["a", "b", "c"]);
}

#[test]
fn setline_below_one_fails_without_writing() {
    // set_buffer_lines: `lnum < 1` reports FAIL before any write.
    let (result, buffer) = buffer_call(&["a", "b"], "setline", &[number(0), text("x")]);
    assert_eq!(result.unwrap(), number(1));
    assert_eq!(buffer.lines, vec!["a", "b"]);
}

#[test]
fn setline_empty_list_always_succeeds_and_writes_nothing() {
    // set_buffer_lines: "not appending anything always succeeds".
    let (result, buffer) = buffer_call(&["a"], "setline", &[number(1), texts(&[])]);
    assert_eq!(result.unwrap(), number(0));
    assert_eq!(buffer.lines, vec!["a"]);
}

#[test]
fn setline_list_replaces_then_appends_consecutive_lines() {
    // builtin.txt setline(): equivalent to one setline() per item, so line
    // 2 of three is replaced and items past the end are appended.
    let (result, buffer) = buffer_call(
        &["a", "b", "c"],
        "setline",
        &[number(2), texts(&["x", "y", "z"])],
    );
    assert_eq!(result.unwrap(), number(0));
    assert_eq!(buffer.lines, vec!["a", "x", "y", "z"]);
}

#[test]
fn setline_list_starting_out_of_bounds_fails_unchanged() {
    // The loop's first iteration hits `lnum > ml_line_count + 1`.
    let (result, buffer) = buffer_call(
        &["a", "b", "c"],
        "setline",
        &[number(9), texts(&["x", "y"])],
    );
    assert_eq!(result.unwrap(), number(1));
    assert_eq!(buffer.lines, vec!["a", "b", "c"]);
}

#[test]
fn setline_appends_list_into_empty_tail_starting_at_last_plus_one() {
    // A 1-line buffer growing to three: first item appends, later items
    // append behind it because the count grows with each write.
    let (result, buffer) = buffer_call(&["a"], "setline", &[number(2), texts(&["x", "y"])]);
    assert_eq!(result.unwrap(), number(0));
    assert_eq!(buffer.lines, vec!["a", "x", "y"]);
}

#[test]
fn setline_converts_non_string_types_like_string() {
    // typval_tostring(_, false): non-Strings use their string() rendering.
    let (result, buffer) = buffer_call(&["a"], "setline", &[number(1), number(42)]);
    assert_eq!(result.unwrap(), number(0));
    assert_eq!(buffer.lines, vec!["42"]);
    // A List nested as the single item of the outer list renders like
    // string(): "[7, 8]".
    let nested = Typval::list(vec![list(&[7, 8])]);
    let (result, buffer) = buffer_call(&["a"], "setline", &[number(1), nested]);
    assert_eq!(result.unwrap(), number(0));
    assert_eq!(buffer.lines, vec!["[7, 8]"]);
}

#[test]
fn setline_dollar_address_targets_last_line() {
    // tv_get_lnum → var2fpos("$") resolves to the last line.
    let (result, buffer) = buffer_call(&["a", "b", "c"], "setline", &[text("$"), text("x")]);
    assert_eq!(result.unwrap(), number(0));
    assert_eq!(buffer.lines, vec!["a", "b", "x"]);
}

#[test]
fn getline_single_line_returns_string() {
    let (result, buffer) = buffer_call(&["a", "b", "c"], "getline", &[number(2)]);
    assert_eq!(result.unwrap(), text("b"));
    assert_eq!(buffer.lines, vec!["a", "b", "c"]);
}

#[test]
fn getline_out_of_range_single_yields_empty_string() {
    // builtin.txt getline(): "smaller than 1 or bigger than the number of
    // lines in the buffer, an empty string is returned".
    let (result, _) = buffer_call(&["a", "b"], "getline", &[number(0)]);
    assert_eq!(result.unwrap(), text(""));
    let (result, _) = buffer_call(&["a", "b"], "getline", &[number(9)]);
    assert_eq!(result.unwrap(), text(""));
}

#[test]
fn getline_range_returns_inclusive_list() {
    let (result, _) = buffer_call(&["a", "b", "c"], "getline", &[number(1), number(3)]);
    let expected = Typval::list(vec![text("a"), text("b"), text("c")]);
    assert_eq!(result.unwrap(), expected);
}

#[test]
fn getline_range_clamps_and_omits_missing_lines() {
    // get_buffer_lines: start clamps up to 1, end clamps down to
    // ml_line_count; non-existing lines are silently omitted.
    let (result, _) = buffer_call(&["a", "b", "c"], "getline", &[number(0), number(2)]);
    let expected = Typval::list(vec![text("a"), text("b")]);
    assert_eq!(result.unwrap(), expected);
    let (result, _) = buffer_call(&["a", "b", "c"], "getline", &[number(2), number(99)]);
    let expected = Typval::list(vec![text("b"), text("c")]);
    assert_eq!(result.unwrap(), expected);
}

#[test]
fn getline_inverted_or_negative_range_yields_empty_list() {
    // get_buffer_lines: `start < 0 || end < start` → empty List.
    let (result, _) = buffer_call(&["a", "b", "c"], "getline", &[number(3), number(2)]);
    assert_eq!(result.unwrap(), texts(&[]));
    let (result, _) = buffer_call(&["a", "b", "c"], "getline", &[number(-1), number(2)]);
    assert_eq!(result.unwrap(), texts(&[]));
}

#[test]
fn getline_dollar_address_reads_last_line() {
    let (result, _) = buffer_call(&["a", "b", "c"], "getline", &[text("$")]);
    assert_eq!(result.unwrap(), text("c"));
    let (result, _) = buffer_call(&["a", "b", "c"], "getline", &[number(2), text("$")]);
    let expected = Typval::list(vec![text("b"), text("c")]);
    assert_eq!(result.unwrap(), expected);
}

// tv_get_lnum: after a non-positive numeric conversion of a String, the
// address translates through var2fpos — "." is the cursor, "'x" a mark.
#[test]
fn getline_dot_and_mark_addresses_translate_through_the_seam() {
    let mut buffer = FakeBuffer::new(&["a", "b", "c"]);
    buffer.cursor = Some(2);
    buffer.marks = vec![('a', 3)];
    let result = crate::builtins::call_buffer_builtin(&mut buffer, "getline", &[text(".")]);
    assert_eq!(result.unwrap(), text("b"));
    let result = crate::builtins::call_buffer_builtin(&mut buffer, "getline", &[text("'a")]);
    assert_eq!(result.unwrap(), text("c"));
    let result =
        crate::builtins::call_buffer_builtin(&mut buffer, "getline", &[text("'a"), text("$")]);
    assert_eq!(result.unwrap(), Typval::list(vec![text("c")]));
}

#[test]
fn getline_unresolved_address_degrades_to_zero() {
    // var2fpos returns NULL for an unset mark or an unknown address; the
    // lnum stays 0 and getline("'z") reads no line.
    let mut buffer = FakeBuffer::new(&["a", "b"]);
    buffer.cursor = None;
    let result = crate::builtins::call_buffer_builtin(&mut buffer, "getline", &[text("'z")]);
    assert_eq!(result.unwrap(), text(""));
    let result = crate::builtins::call_buffer_builtin(&mut buffer, "getline", &[text("w0")]);
    assert_eq!(result.unwrap(), text(""));
}

#[test]
fn setline_accepts_string_addresses() {
    let mut buffer = FakeBuffer::new(&["a", "b", "c"]);
    buffer.cursor = Some(2);
    let result =
        crate::builtins::call_buffer_builtin(&mut buffer, "setline", &[text("."), text("x")]);
    assert_eq!(result.unwrap(), number(0));
    assert_eq!(buffer.lines, vec!["a", "x", "c"]);
    // An unresolvable address keeps the failure result of lnum 0.
    let result =
        crate::builtins::call_buffer_builtin(&mut buffer, "setline", &[text("'z"), text("y")]);
    assert_eq!(result.unwrap(), number(1));
    assert_eq!(buffer.lines, vec!["a", "x", "c"]);
}

#[test]
fn buffer_builtins_check_arity_from_generated_specs() {
    // eval.lua rows: getline {1,2}, setline {2,2} → E119/E118.
    let (result, _) = buffer_call(&["a"], "getline", &[]);
    assert_eq!(result.unwrap_err().code, "E119");
    let (result, _) = buffer_call(&["a"], "setline", &[number(1)]);
    assert_eq!(result.unwrap_err().code, "E119");
    let (result, _) = buffer_call(&["a"], "setline", &[number(1), text("x"), number(3)]);
    assert_eq!(result.unwrap_err().code, "E118");
}

#[test]
fn typval_dispatcher_leaves_buffer_builtins_unimplemented() {
    // `Builtins` alone has no buffer; only hosts routing through
    // `call_buffer_builtin` serve getline/setline.
    assert!(crate::builtins::is_buffer_builtin("getline"));
    assert!(crate::builtins::is_buffer_builtin("setline"));
    assert!(!crate::builtins::is_buffer_builtin("getbufline"));
    let error = call("setline", vec![number(1), text("x")]).unwrap_err();
    assert!(matches!(
        error.kind,
        crate::EvalErrorKind::NotImplemented(_)
    ));
}

#[test]
fn fnamemodify_obeys_filename_modifier_order() {
    // cmdline.txt `filename-modifiers`: :h/:t and repeated :r/:e.
    assert_eq!(
        call("fnamemodify", vec![text("src/archive.tar.gz"), text(":h")]).unwrap(),
        text("src")
    );
    assert_eq!(
        call(
            "fnamemodify",
            vec![text("src/archive.tar.gz"), text(":t:r:r")]
        )
        .unwrap(),
        text("archive")
    );
    assert_eq!(
        call(
            "fnamemodify",
            vec![text("src/archive.tar.gz"), text(":e:e")]
        )
        .unwrap(),
        text("tar.gz")
    );
    assert_eq!(
        call("fnamemodify", vec![text(".nvimrc"), text(":r")]).unwrap(),
        text(".nvimrc")
    );
    assert_eq!(
        call("fnamemodify", vec![text("src/"), text(":h")]).unwrap(),
        text("src")
    );
    assert_eq!(
        call("fnamemodify", vec![text("src/"), text(":t")]).unwrap(),
        text("")
    );
    assert_eq!(
        call("fnamemodify", vec![text("src/x"), text(":8:t")]).unwrap(),
        text("x")
    );
    assert_eq!(
        call("fnamemodify", vec![text(""), text(":h")]).unwrap(),
        text(".")
    );
}

#[test]
fn fnamemodify_full_relative_and_home_forms() {
    let current = std::env::current_dir().unwrap();
    let absolute = call("fnamemodify", vec![text("src/file.rs"), text(":p")]).unwrap();
    assert_eq!(
        absolute,
        text(&current.join("src/file.rs").to_string_lossy())
    );
    assert_eq!(
        call("fnamemodify", vec![absolute.clone(), text(":.")]).unwrap(),
        text("src/file.rs")
    );
    if let Some(home) = std::env::var_os("HOME") {
        let path = std::path::PathBuf::from(home).join("file");
        assert_eq!(
            call(
                "fnamemodify",
                vec![text(&path.to_string_lossy()), text(":~")]
            )
            .unwrap(),
            text("~/file")
        );
    }
    assert_eq!(
        call("fnamemodify", vec![text("file"), text(":unsupported")]).unwrap(),
        text("file")
    );
}

#[test]
fn fnamemodify_substitutions_use_regex_seam() {
    let regex = LiteralRegex {
        calls: Cell::new(0),
    };
    let mut builtins = Builtins::new(&regex);
    let mut scope = Scope::new();
    assert_eq!(
        builtins
            .call(
                &OxStr::from("fnamemodify"),
                vec![text("src/version.c"), text(":s?version?main?")],
                &mut scope
            )
            .unwrap(),
        text("src/main.c")
    );
    assert_eq!(
        builtins
            .call(
                &OxStr::from("fnamemodify"),
                vec![text("a/a/a"), text(":gs?a?b?")],
                &mut scope
            )
            .unwrap(),
        text("b/b/b")
    );
    assert_eq!(regex.calls.get(), 2);
}

#[test]
fn simplify_preserves_only_explicit_current_directory_prefixes() {
    for (input, expected) in [
        ("./dir/.././/file/", "./file/"),
        ("this/./is//redundant/../../../foo", "foo"),
        ("a/../b", "b"),
        ("dir/.././file", "./file"),
        ("a/./../b", "b"),
        ("./a/../b", "./b"),
        ("./b/..", "."),
        ("./b/../", "./"),
        ("./../b", "../b"),
        ("../a/..", ".."),
        ("/a/../../b", "/b"),
        ("//a/../b", "//b"),
        ("///one//two/../three", "/one/three"),
    ] {
        assert_eq!(
            call("simplify", vec![text(input)]).unwrap(),
            text(expected),
            "input: {input}",
        );
    }
}

#[test]
fn resolve_uses_real_file_types() {
    let root = std::env::temp_dir().join(format!("ox-eval-path-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("directory")).unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.join("directory"), root.join("link")).unwrap();
        assert_eq!(
            call("resolve", vec![text(&root.join("link/").to_string_lossy())]).unwrap(),
            text(&format!("{}/", root.join("directory").to_string_lossy()))
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn getcwd_accepts_upstream_optional_numeric_selectors() {
    let expected = text(&std::env::current_dir().unwrap().to_string_lossy());
    assert_eq!(call("getcwd", vec![]).unwrap(), expected);
    assert_eq!(
        call("getcwd", vec![number(-1), number(-1), number(-1)]).unwrap(),
        expected
    );
}

#[cfg(unix)]
#[test]
fn executable_requires_execute_permission_and_regular_file() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = std::env::temp_dir().join(format!("ox-eval-executable-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let program = root.join("program");
    std::fs::write(&program, b"#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        call("executable", vec![text(&program.to_string_lossy())]).unwrap(),
        number(0)
    );
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        call("executable", vec![text(&program.to_string_lossy())]).unwrap(),
        number(1)
    );
    assert_eq!(call("executable", vec![text("sh")]).unwrap(), number(1));
    std::fs::remove_dir_all(root).unwrap();
}

// f_printf: strings.c C-style formatting with flags, width, precision.
#[test]
fn printf_formats_strings_numbers_and_radices() {
    assert_eq!(
        call("printf", vec![text("Screen (%u lines)"), number(42)]).unwrap(),
        text("Screen (42 lines)")
    );
    assert_eq!(
        call("printf", vec![text("<%x>"), number(255)]).unwrap(),
        text("<ff>")
    );
    assert_eq!(
        call(
            "printf",
            vec![text("%5.2d|%-5s|%05d"), number(3), text("ab"), number(42)]
        )
        .unwrap(),
        text("   03|ab   |00042")
    );
    assert_eq!(
        call("printf", vec![text("100%% and %s"), text("done")]).unwrap(),
        text("100% and done")
    );
    assert!(call("printf", vec![text("%d %d"), number(1)]).is_err());
}

#[test]
fn exists_checks_evaluator_owned_namespaces_without_mutation() {
    let mut builtins = Builtins::without_regex();
    let mut scope = Scope::new();
    scope
        .set_scoped(
            crate::scope::ScopeKind::Global,
            b"answer",
            0,
            Typval::Number(42),
        )
        .unwrap();
    scope.set_option(
        crate::scope::OptionScope::Global,
        b"number",
        Typval::Number(1),
    );

    for (query, expected) in [
        ("g:answer", 1),
        ("g:missing", 0),
        ("&number", 1),
        ("&g:number", 1),
        ("&missing", 0),
        ("*printf", 1),
        ("*DefinitelyMissing", 0),
        ("v:true", 1),
        ("", 0),
    ] {
        assert_eq!(
            builtins
                .call(&OxStr::from("exists"), vec![text(query)], &mut scope)
                .unwrap(),
            Typval::Number(expected),
            "query {query}",
        );
    }
}

// `indexof()` — oracle: test/old/testdir/test_listdict.vim Test_indexof and
// test_blob.vim Test_indexof.
#[test]
// Comprehensive indexof test covering string callback, start index, and edge cases.
#[allow(clippy::too_many_lines)]
fn indexof_matches_upstream_string_callback_and_startidx() {
    let values = Typval::list(vec![number(10), number(10), number(20)]);
    assert_eq!(
        call("indexof", vec![values.clone(), text("v:val == 10")]).unwrap(),
        number(0)
    );
    assert_eq!(
        call("indexof", vec![values.clone(), text("v:val == 20")]).unwrap(),
        number(2)
    );
    assert_eq!(
        call("indexof", vec![values.clone(), text("v:val == 30")]).unwrap(),
        number(-1)
    );
    let opts = |start: i64| Typval::dict(vec![(OxStr::from("startidx"), number(start))]);
    assert_eq!(
        call(
            "indexof",
            vec![values.clone(), text("v:val == 10"), opts(0)]
        )
        .unwrap(),
        number(0)
    );
    assert_eq!(
        call(
            "indexof",
            vec![values.clone(), text("v:val == 10"), opts(-2)]
        )
        .unwrap(),
        number(1)
    );
    assert_eq!(
        call(
            "indexof",
            vec![values.clone(), text("v:val == 10"), opts(4)]
        )
        .unwrap(),
        number(-1)
    );
    assert_eq!(
        call(
            "indexof",
            vec![values.clone(), text("v:val == 10"), opts(-4)]
        )
        .unwrap(),
        number(-1)
    );
    assert_eq!(
        call(
            "indexof",
            vec![values.clone(), text("v:val == 10"), Typval::dict(vec![])]
        )
        .unwrap(),
        number(0)
    );
    // Empty and null-string callbacks never match (funcs.c 2972-2975).
    assert_eq!(
        call("indexof", vec![values.clone(), text("")]).unwrap(),
        number(-1)
    );
    assert_eq!(
        call(
            "indexof",
            vec![values.clone(), Typval::Special(Special::Null)]
        )
        .unwrap(),
        number(-1)
    );
    // A String result converts as a number, so a non-numeric string is 0.
    let strings = Typval::list(vec![text("a"), text("b"), text("c")]);
    let blob = Typval::Blob(vec![0xde, 0xad, 0x01, 0xef]);
    assert_eq!(
        call("indexof", vec![blob.clone(), text("v:val == 0xef")]).unwrap(),
        number(3)
    );
    assert_eq!(
        call("indexof", vec![blob.clone(), text("v:val == 0xff")]).unwrap(),
        number(-1)
    );
    assert_eq!(
        call("indexof", vec![blob, text("v:val == 0x01"), opts(-2)]).unwrap(),
        number(2)
    );
    // Type errors: E1226 container, E1256 callback, E1206 opts.
    assert_eq!(
        call("indexof", vec![Typval::dict(vec![]), text("v:val == 2")])
            .unwrap_err()
            .code,
        "E1226"
    );
    assert_eq!(
        call("indexof", vec![values.clone(), Typval::dict(vec![])])
            .unwrap_err()
            .code,
        "E1256"
    );
    assert_eq!(
        call(
            "indexof",
            vec![values, text("v:val == 2"), Typval::list(vec![])]
        )
        .unwrap_err()
        .code,
        "E1206"
    );
    // Callback errors abort the search and surface (upstream did_emsg check).
    assert_eq!(
        call("indexof", vec![strings, text("v:val == 'b'")]).unwrap(),
        number(1)
    );
    assert_eq!(
        call("indexof", vec![Typval::list(vec![]), text("v:val == 1")]).unwrap(),
        number(-1)
    );
}

#[test]
fn matchstrpos_and_matchlist_shape_results() {
    let regex = LiteralRegex {
        calls: Cell::new(0),
    };
    let mut builtins = Builtins::new(&regex);
    assert_eq!(
        builtins
            .call(
                &OxStr::from("matchstrpos"),
                vec![text("testing"), text("ing")],
                &mut Scope::new()
            )
            .unwrap(),
        Typval::list(vec![text("ing"), number(4), number(7)])
    );
    assert_eq!(
        builtins
            .call(
                &OxStr::from("matchstrpos"),
                vec![
                    Typval::list(vec![text("vim"), text("testing")]),
                    text("ing")
                ],
                &mut Scope::new()
            )
            .unwrap(),
        Typval::list(vec![text("ing"), number(1), number(4), number(7)])
    );
    assert_eq!(
        builtins
            .call(
                &OxStr::from("matchlist"),
                vec![text("acd"), text("acd")],
                &mut Scope::new()
            )
            .unwrap(),
        Typval::list(vec![
            text("acd"),
            text(""),
            text(""),
            text(""),
            text(""),
            text(""),
            text(""),
            text(""),
            text(""),
            text("")
        ])
    );
}

#[test]
fn matchstrlist_reports_each_matching_string_and_validates_options() {
    let regex = LiteralRegex {
        calls: Cell::new(0),
    };
    let mut builtins = Builtins::new(&regex);
    let result = builtins
        .call(
            &OxStr::from("matchstrlist"),
            vec![
                Typval::list(vec![text("about"), text("above"), text("other")]),
                text("bo"),
            ],
            &mut Scope::new(),
        )
        .unwrap();
    assert_eq!(
        result,
        Typval::list(vec![
            Typval::dict(vec![
                (OxStr::from("idx"), number(0)),
                (OxStr::from("byteidx"), number(1)),
                (OxStr::from("text"), text("bo"))
            ]),
            Typval::dict(vec![
                (OxStr::from("idx"), number(1)),
                (OxStr::from("byteidx"), number(1)),
                (OxStr::from("text"), text("bo"))
            ]),
        ])
    );

    let error = builtins
        .call(
            &OxStr::from("matchstrlist"),
            vec![
                Typval::list(vec![text("abc")]),
                text("a"),
                Typval::list(vec![]),
            ],
            &mut Scope::new(),
        )
        .unwrap_err();
    assert_eq!(error.code, "E1206");
}

#[test]
fn small_process_and_path_builtins_match_vim_contracts() {
    assert_eq!(
        call("getpid", vec![]).unwrap(),
        number(i64::from(std::process::id()))
    );
    let Typval::String(hostname) = call("hostname", vec![]).unwrap() else {
        panic!("hostname must return a String")
    };
    assert!(!hostname.as_bytes().is_empty());
    assert_eq!(
        call("gettext", vec![text("message")]).unwrap(),
        text("message")
    );
    assert_eq!(call("gettext", vec![number(1)]).unwrap_err().code, "E1174");
    assert_eq!(
        call("isabsolutepath", vec![text("/tmp")]).unwrap(),
        number(1)
    );
    assert_eq!(
        call("isabsolutepath", vec![text("../tmp")]).unwrap(),
        number(0)
    );
}

#[test]
fn slice_uses_exclusive_bounds_and_clamps_negative_indices() {
    assert_eq!(
        call(
            "slice",
            vec![
                Typval::list((0..6).map(number).collect()),
                number(2),
                number(4)
            ]
        )
        .unwrap(),
        Typval::list(vec![number(2), number(3)])
    );
    assert_eq!(
        call("slice", vec![text("012345"), number(1), number(-1)]).unwrap(),
        text("1234")
    );
    assert_eq!(
        call("slice", vec![Typval::Blob(vec![0, 1, 2, 3]), number(-3)]).unwrap(),
        Typval::Blob(vec![1, 2, 3])
    );
}

#[test]
fn insert_rejects_negative_list_indices() {
    let values = Typval::list(vec![number(1)]);
    assert_eq!(
        call("insert", vec![values, number(2), number(-1)])
            .unwrap_err()
            .code,
        "E686"
    );
}

#[test]
fn measured_string_semantics_match_oldtest_cases() {
    assert_eq!(
        call("str2nr", vec![text("1'000''000"), number(10), number(1)]).unwrap(),
        number(1000)
    );
    assert_eq!(
        call("reverse", vec![text("a\u{301}b")]).unwrap(),
        text("ba\u{301}")
    );
    assert_eq!(call("reverse", vec![text("🇦🇧🇨")]).unwrap(), text("🇨🇦🇧"));
    assert_eq!(
        call("strpart", vec![text("abcdefg"), number(-2), number(4)]).unwrap(),
        text("ab")
    );
    assert_eq!(
        call(
            "strpart",
            vec![text("co\u{301}mposed"), number(1), number(1), number(1)]
        )
        .unwrap(),
        text("o\u{301}")
    );
}

fn strings(values: &[&str]) -> Typval {
    Typval::list(values.iter().map(|value| text(value)).collect())
}

/// Oracle: `.references/neovim/build/bin/nvim` (v0.13.0-dev-1375+g8e0e19c08b)
/// evaluating the same expressions, plus `runtime/doc/builtin.txt`
/// `matchfuzzy()`.
#[test]
fn matchfuzzy_ranks_by_upstream_fzy_score() {
    assert_eq!(
        call(
            "matchfuzzy",
            vec![strings(&["clay", "crow", "hello"]), text("cay")]
        )
        .unwrap(),
        strings(&["clay"])
    );
    // Space-separated words are matched independently unless "matchseq".
    assert_eq!(
        call(
            "matchfuzzy",
            vec![strings(&["hello world", "world hello"]), text("wo he")]
        )
        .unwrap(),
        strings(&["hello world", "world hello"])
    );
    assert_eq!(
        call(
            "matchfuzzy",
            vec![
                strings(&["hello world", "world hello"]),
                text("wo he"),
                Typval::dict(vec![(OxStr::from("matchseq"), number(1))]),
            ]
        )
        .unwrap(),
        strings(&["world hello"])
    );
    // Path- and word-separator bonuses order these three.
    assert_eq!(
        call(
            "matchfuzzy",
            vec![strings(&["foo/bar", "foobar", "fooxbar"]), text("fb")]
        )
        .unwrap(),
        strings(&["foo/bar", "foobar", "fooxbar"])
    );
}

#[test]
fn matchfuzzypos_reports_positions_and_scores() {
    assert_eq!(
        call(
            "matchfuzzypos",
            vec![strings(&["clay", "crow", "hello"]), text("cay")]
        )
        .unwrap(),
        Typval::list(vec![
            strings(&["clay"]),
            Typval::list(vec![list(&[0, 2, 3])]),
            list(&[1890])
        ])
    );
    assert_eq!(
        call(
            "matchfuzzypos",
            vec![strings(&["curdir/curfile"]), text("cf")]
        )
        .unwrap(),
        Typval::list(vec![
            strings(&["curdir/curfile"]),
            Typval::list(vec![list(&[7, 10])]),
            list(&[830])
        ])
    );
    // An exact ignoring-case match short-circuits to the maximum score.
    assert_eq!(
        call("matchfuzzypos", vec![strings(&["ab"]), text("ab")]).unwrap(),
        Typval::list(vec![
            strings(&["ab"]),
            Typval::list(vec![list(&[0, 1])]),
            list(&[i64::from(i32::MAX)])
        ])
    );
    // Boundary: an empty pattern matches nothing, but the three lists exist.
    assert_eq!(
        call("matchfuzzypos", vec![strings(&["x"]), text("")]).unwrap(),
        Typval::list(vec![
            Typval::list(vec![]),
            Typval::list(vec![]),
            Typval::list(vec![])
        ])
    );
}

#[test]
fn matchfuzzy_reads_dict_items_through_key_and_limits_matches() {
    let items = Typval::list(vec![
        Typval::dict(vec![(OxStr::from("n"), text("clay"))]),
        Typval::dict(vec![(OxStr::from("n"), text("crow"))]),
    ]);
    let options = Typval::dict(vec![(OxStr::from("key"), text("n"))]);
    assert_eq!(
        call("matchfuzzy", vec![items, text("cay"), options]).unwrap(),
        Typval::list(vec![Typval::dict(vec![(OxStr::from("n"), text("clay"))])])
    );
    // Items that yield no string are skipped, not rejected.
    assert_eq!(
        call(
            "matchfuzzy",
            vec![
                Typval::list(vec![number(1), number(2), text("a1")]),
                text("1")
            ]
        )
        .unwrap(),
        strings(&["a1"])
    );
    let limited = Typval::dict(vec![(OxStr::from("limit"), number(2))]);
    assert_eq!(
        call(
            "matchfuzzy",
            vec![strings(&["ab", "abc", "abcd"]), text("ab"), limited]
        )
        .unwrap(),
        strings(&["ab", "abc"])
    );
}

#[test]
fn matchfuzzy_rejects_bad_arguments_with_upstream_codes() {
    assert_eq!(
        call("matchfuzzy", vec![text("abc"), text("a")])
            .unwrap_err()
            .message,
        "Argument of matchfuzzy() must be a List"
    );
    assert_eq!(
        call("matchfuzzypos", vec![text("abc"), text("a")])
            .unwrap_err()
            .code,
        "E686"
    );
    assert_eq!(
        call("matchfuzzy", vec![list(&[1, 2]), number(3)])
            .unwrap_err()
            .message,
        "Invalid argument: 3"
    );
    assert_eq!(
        call("matchfuzzy", vec![list(&[1, 2]), list(&[])])
            .unwrap_err()
            .code,
        "E730"
    );
    assert_eq!(
        call("matchfuzzy", vec![strings(&["a"]), text("a"), number(3)])
            .unwrap_err()
            .code,
        "E1206"
    );
    let bad_key = Typval::dict(vec![(OxStr::from("key"), number(3))]);
    assert_eq!(
        call("matchfuzzy", vec![strings(&["a"]), text("a"), bad_key])
            .unwrap_err()
            .message,
        "Invalid value for argument key: 3"
    );
    let bad_cb = Typval::dict(vec![(OxStr::from("text_cb"), number(0))]);
    assert_eq!(
        call("matchfuzzy", vec![strings(&["a"]), text("a"), bad_cb])
            .unwrap_err()
            .code,
        "E6000"
    );
    let bad_limit = Typval::dict(vec![(OxStr::from("limit"), text("x"))]);
    assert_eq!(
        call("matchfuzzy", vec![strings(&["a"]), text("a"), bad_limit])
            .unwrap_err()
            .message,
        "Invalid value for argument limit"
    );
    assert_eq!(
        call("matchfuzzy", vec![strings(&["a"])]).unwrap_err().code,
        "E119"
    );
    assert_eq!(
        call(
            "matchfuzzy",
            vec![
                strings(&["a"]),
                text("a"),
                Typval::dict(vec![]),
                Typval::dict(vec![])
            ]
        )
        .unwrap_err()
        .code,
        "E118"
    );
}

/// Every row was produced by `.references/neovim/build/bin/nvim`
/// (v0.13.0-dev-1375+g8e0e19c08b) evaluating
/// `matchfuzzypos({items}, {pattern})` and rendering it with `string()`.
/// These pin the fzy weights, the separator and capital bonuses, the leading
/// and trailing gap penalties, and the non-ASCII character indexing.
#[test]
fn matchfuzzypos_matches_upstream_scores_verbatim() {
    let cases: &[(&[&str], &str, &str)] = &[
        (&["ab"], "ab", "[['ab'], [[0, 1]], [2147483647]]"),
        (&["Xy"], "xy", "[['Xy'], [[0, 1]], [2147483647]]"),
        (
            &["hello-world", "hello_world", "hello.world", "HelloWorld"],
            "hw",
            "[['hello-world', 'hello_world', 'HelloWorld', 'hello.world'], [[0, 6], [0, 6], [0, 5], [0, 6]], [1630, 1630, 1540, 1430]]",
        ),
        (
            &["a/b/c/d"],
            "abcd",
            "[['a/b/c/d'], [[0, 2, 4, 6]], [3570]]",
        ),
        (
            &["xxxxxxxxxxab"],
            "ab",
            "[['xxxxxxxxxxab'], [[10, 11]], [950]]",
        ),
        (
            &["abxxxxxxxxxx"],
            "ab",
            "[['abxxxxxxxxxx'], [[0, 1]], [1850]]",
        ),
        (&["Ünïcödé"], "nc", "[['Ünïcödé'], [[1, 3]], [-30]]"),
        (&["café"], "cf", "[['café'], [[0, 2]], [885]]"),
        (&["ababab"], "ab", "[['ababab'], [[0, 1]], [1880]]"),
    ];
    for (items, pattern, expected) in cases {
        let list = Typval::list(items.iter().map(|value| text(value)).collect());
        let matched = call("matchfuzzypos", vec![list, text(pattern)]).unwrap();
        assert_eq!(
            call("string", vec![matched]).unwrap(),
            text(expected),
            "pattern {pattern}"
        );
    }
}

/// Oracle: `nvim -u NONE --headless -c 'echo tempname()'` yields
/// `/tmp/nvim.<user>/<random>/0` then `.../1`, with the parent directory
/// present at mode 0700 and the name itself not created.
#[test]
fn tempname_returns_unique_names_inside_a_private_directory() {
    let Typval::String(first) = call("tempname", vec![]).unwrap() else {
        panic!("String expected")
    };
    let Typval::String(second) = call("tempname", vec![]).unwrap() else {
        panic!("String expected")
    };
    assert_ne!(first, second);

    let first = std::path::PathBuf::from(first.to_string_lossy().into_owned());
    let second = std::path::PathBuf::from(second.to_string_lossy().into_owned());
    assert!(!first.exists(), "tempname() must not create the file");
    assert_eq!(first.parent(), second.parent());
    let parent = first
        .parent()
        .expect("a tempname is never a filesystem root");
    assert!(parent.is_dir(), "the containing directory is created");

    // Boundary: the last component is a decimal counter that advances by one.
    let counter = |path: &std::path::Path| {
        path.file_name()
            .expect("a trailing name")
            .to_string_lossy()
            .parse::<u64>()
            .expect("a decimal counter")
    };
    assert_eq!(counter(&second), counter(&first) + 1);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = parent
            .metadata()
            .expect("a readable directory")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o700,
            "only this user may read the temporary directory"
        );
    }

    // Documented error: tempname() takes no arguments.
    assert_eq!(call("tempname", vec![number(1)]).unwrap_err().code, "E118");
}

/// A real Vim regex, so the glob-to-regex conversion inside the path search
/// is exercised by the engine the editor installs rather than by a stub.
struct VimRegex;

impl RegexEngine for VimRegex {
    fn is_match(&self, text: &OxStr, pattern: &OxStr, ignore_case: bool) -> crate::Result<bool> {
        let source = if ignore_case {
            format!("\\c{}", pattern.to_string_lossy())
        } else {
            pattern.to_string_lossy().into_owned()
        };
        let program = ox_regex::compile(&source, ox_regex::Magic::Magic)
            .map_err(|error| crate::EvalError::new("E54", 0, error.to_string()))?;
        Ok(ox_regex::exec(
            &program,
            &ox_regex::Text::new(text.to_string_lossy().into_owned()),
        )
        .is_some())
    }
}

/// The `test_findfile.vim` tree, rebuilt from scratch so the test does not
/// depend on leftovers, at an absolute root so it is independent of the
/// process-wide current directory.
fn findfile_fixture() -> std::path::PathBuf {
    let root = std::env::temp_dir().join("ox-eval-findfile-fixture");
    let _ = std::fs::remove_dir_all(&root);
    let base = root.join("Xfinddir1");
    std::fs::create_dir_all(base.join("Xdir2/Xdir3/Xdir2"))
        .expect("a writable temporary directory");
    for name in [
        "foo",
        "bar",
        "Xdir2/foo",
        "Xdir2/foobar",
        "Xdir2/Xdir3/bar",
        "Xdir2/Xdir3/barfoo",
    ] {
        std::fs::write(base.join(name), b"").expect("a writable fixture file");
    }
    root
}

/// Every expectation came from `.references/neovim/build/bin/nvim`
/// (v0.13.0-dev-1375+g8e0e19c08b) evaluating the same call over the same
/// tree from a current directory outside it, so results stay absolute.
#[test]
fn findfile_and_finddir_match_upstream_over_the_oldtest_tree() {
    let root = findfile_fixture();
    let directory = root.join("Xfinddir1").to_string_lossy().into_owned();
    let cases: &[(&str, &str, &str, i64, &str)] = &[
        ("findfile", "foo", "{d}", 1, "'{d}/foo'"),
        ("findfile", "bar", "{d}", 1, "'{d}/bar'"),
        ("findfile", "foobar", "{d}", 1, "''"),
        // A directory is never a findfile() result; finddir() finds it.
        ("findfile", "Xdir2", "{d}", 1, "''"),
        ("finddir", "Xdir2", "{d}", 1, "'{d}/Xdir2'"),
        ("findfile", "foo", "{d}/*", 1, "'{d}/Xdir2/foo'"),
        ("findfile", "bar", "{d}/*", 1, "''"),
        ("findfile", "bar", "{d}/*/*", 1, "'{d}/Xdir2/Xdir3/bar'"),
        ("findfile", "bar", "{d}/**", 1, "'{d}/bar'"),
        (
            "findfile",
            "bar",
            "{d}/**/Xdir3",
            1,
            "'{d}/Xdir2/Xdir3/bar'",
        ),
        // The count after "**" caps the descent.
        (
            "findfile",
            "barfoo",
            "{d}/**2",
            1,
            "'{d}/Xdir2/Xdir3/barfoo'",
        ),
        ("findfile", "barfoo", "{d}/**1", 1, "''"),
        ("findfile", "foobar", "{d}/**1", 1, "'{d}/Xdir2/foobar'"),
        // {count}: 0 and 1 both mean the first match; too large yields none.
        ("findfile", "bar", "{d}/**", 0, "'{d}/bar'"),
        ("findfile", "bar", "{d}/**", 2, "'{d}/Xdir2/Xdir3/bar'"),
        ("findfile", "bar", "{d}/**", 3, "''"),
        (
            "findfile",
            "bar",
            "{d}/**",
            -1,
            "['{d}/bar', '{d}/Xdir2/Xdir3/bar']",
        ),
        (
            "finddir",
            "Xdir2",
            "{d}/**",
            -1,
            "['{d}/Xdir2', '{d}/Xdir2/Xdir3/Xdir2']",
        ),
        // The fixed part ends at the first '*', so "Xdir*" leaves an
        // absolute start directory that does not exist and finds nothing.
        ("findfile", "bar", "{d}/Xdir*/Xdir3", 1, "''"),
        ("findfile", "bar", "{d}/*2/*3", 1, "'{d}/Xdir2/Xdir3/bar'"),
        (
            "findfile",
            "foo",
            "{d},{d}/Xdir2",
            -1,
            "['{d}/foo', '{d}/Xdir2/foo']",
        ),
        // Upward search, with and without a stop directory.
        (
            "findfile",
            "bar",
            "{d}/Xdir2/Xdir3;{d}",
            -1,
            "['{d}/Xdir2/Xdir3/bar', '{d}/bar']",
        ),
        (
            "findfile",
            "bar",
            "{d}/Xdir2/Xdir3;",
            -1,
            "['{d}/Xdir2/Xdir3/bar', '{d}/bar']",
        ),
    ];

    let regex = VimRegex;
    for (function, name, path, count, expected) in cases {
        let mut builtins = Builtins::new(&regex);
        let arguments = vec![
            text(name),
            text(&path.replace("{d}", &directory)),
            number(*count),
        ];
        let found = builtins
            .call(&OxStr::from(*function), arguments, &mut Scope::new())
            .unwrap_or_else(|error| panic!("{function}({name}, {path}, {count}): {error}"));
        let rendered = call("string", vec![found]).unwrap();
        assert_eq!(
            rendered,
            text(&expected.replace("{d}", &directory)),
            "{function}({name}, {path}, {count})"
        );
    }

    // Documented error: a "**" count must end the entry or precede a '/'.
    let mut builtins = Builtins::new(&regex);
    let bad = vec![text("bar"), text(&format!("{directory}/**2x"))];
    assert_eq!(
        builtins
            .call(&OxStr::from("findfile"), bad, &mut Scope::new())
            .unwrap_err()
            .code,
        "E343"
    );
    // Boundary: an empty name finds nothing, and a negative count still
    // returns a List.
    assert_eq!(call("findfile", vec![text("")]).unwrap(), text(""));
    assert_eq!(
        call("findfile", vec![text(""), text("."), number(-1)]).unwrap(),
        Typval::list(vec![])
    );
}

/// Oracle: `nvim -u NONE --headless` evaluating `findfile('bår.txt', d)` and
/// `finddir('café', d)` over the same tree; both match.
///
/// Regression: `expand_env` must copy the bytes it does not expand
/// unchanged. Re-encoding each byte as its Latin-1 scalar turns `café` into
/// `cafÃ©`, and every non-ASCII name then misses.
#[test]
fn findfile_and_finddir_preserve_non_ascii_name_bytes() {
    let root = std::env::temp_dir().join("ox-eval-findfile-non-ascii");
    let _ = std::fs::remove_dir_all(&root);
    let directory = root.join("café");
    std::fs::create_dir_all(&directory).expect("a writable temporary directory");
    std::fs::write(directory.join("bår.txt"), b"").expect("a writable fixture file");
    let regex = VimRegex;

    let arguments = vec![text("bår.txt"), text(&directory.to_string_lossy())];
    let found = Builtins::new(&regex)
        .call(&OxStr::from("findfile"), arguments, &mut Scope::new())
        .unwrap();
    assert_eq!(found, text(&directory.join("bår.txt").to_string_lossy()));

    let arguments = vec![text("café"), text(&root.to_string_lossy())];
    let found = Builtins::new(&regex)
        .call(&OxStr::from("finddir"), arguments, &mut Scope::new())
        .unwrap();
    assert_eq!(found, text(&directory.to_string_lossy()));
}

/// Oracle: `nvim -u NONE --headless` running `:lockvar`, `:lockvar!`,
/// `:lockvar 1`, `:unlockvar!` and `islocked()` over the same values, and
/// `let` on a locked variable, which reports `E741: Value is locked: g:n`.
/// `E1122` is `var_check_lock`'s text for the variable-name check.
#[test]
fn lockvar_and_unlockvar_apply_depth_and_report_upstream_errors() {
    let nested = list(&[2]);
    let mut scope = Scope::new();
    scope
        .set(b"one", Typval::list(vec![number(1), nested.clone()]))
        .unwrap();
    scope
        .set(b"deep", Typval::list(vec![number(1), nested.clone()]))
        .unwrap();

    // Depth 1 locks only the outer container.
    scope.lockvar(b"one", 1).unwrap();
    assert_eq!(scope.islocked(b"one", 0).unwrap(), 2);
    assert_eq!(crate::is_locked_value(&nested).unwrap(), number(0));

    // A negative depth reaches every nested container, and unlocking with
    // the same depth reverses it.
    scope.lockvar(b"deep", -1).unwrap();
    assert_eq!(crate::is_locked_value(&nested).unwrap(), number(3));
    assert_eq!(
        call("add", vec![nested.clone(), number(9)])
            .unwrap_err()
            .code,
        "E741"
    );
    scope.unlockvar(b"deep", -1).unwrap();
    assert_eq!(scope.islocked(b"deep", 0).unwrap(), 0);
    assert_eq!(crate::is_locked_value(&nested).unwrap(), number(0));
    assert_eq!(call("add", vec![nested, number(9)]).unwrap(), list(&[2, 9]));

    // Depth 0 changes nothing at all.
    scope.set(b"untouched", list(&[1])).unwrap();
    scope.lockvar(b"untouched", 0).unwrap();
    assert_eq!(scope.islocked(b"untouched", 0).unwrap(), 0);

    // The variable-name lock is reported separately from the value lock.
    scope.set(b"scalar", number(1)).unwrap();
    scope.lockvar(b"scalar", 2).unwrap();
    let error = scope.check_variable_lock(b"scalar").unwrap_err();
    assert_eq!(
        (error.code, error.message.as_str()),
        ("E1122", "Variable is locked: scalar")
    );
    let error = scope.check_value_lock(b"one", 0).unwrap_err();
    assert_eq!(
        (error.code, error.message.as_str()),
        ("E741", "Value is locked: one")
    );
    scope.unlockvar(b"scalar", 2).unwrap();
    assert!(scope.check_variable_lock(b"scalar").is_ok());

    // Boundary: an unknown name is silently ignored, as `do_lock_var` is.
    assert!(scope.lockvar(b"missing", 2).is_ok());
    assert!(scope.unlockvar(b"missing", 2).is_ok());
    // Documented error: islocked() still reports an unknown name as E121.
    assert_eq!(scope.islocked(b"missing", 0).unwrap_err().code, "E121");
}

/// Oracle: `nvim -u NONE --headless` running `:lockvar`, `:lockvar 0` and
/// `:unlockvar` and then assigning to the same variable:
///
/// - `lockvar g:n`   then `let g:n = 2`   — `E741: Value is locked: g:n`
/// - `lockvar g:l`   then `let g:l = [2]` — `E741: Value is locked: g:l`
/// - `lockvar s`     then `let s = 6`     — `E741: Value is locked: s`
/// - `lockvar 0 g:n` then `let g:n = 8`   — `E1122: Variable is locked: g:n`
/// - `lockvar 0 g:d` then `add(g:d, 5)`   — accepted, the value is unlocked
/// - `unlockvar g:n` then `let g:n = 7`   — accepted
#[test]
fn assignment_to_a_locked_variable_is_refused_with_upstream_errors() {
    let mut scope = Scope::new();
    scope
        .set_scoped(ScopeKind::Global, b"n", 0, number(1))
        .unwrap();
    scope
        .set_scoped(ScopeKind::Global, b"l", 0, list(&[1]))
        .unwrap();
    scope.set(b"s", number(5)).unwrap();

    // The default depth locks the value too, whatever the value's type, so
    // the value check answers first for all three.
    scope.lockvar(b"g:n", 2).unwrap();
    scope.lockvar(b"g:l", 2).unwrap();
    scope.lockvar(b"s", 2).unwrap();
    let error = scope
        .set_scoped(ScopeKind::Global, b"n", 0, number(2))
        .unwrap_err();
    assert_eq!(
        (error.code, error.message.as_str()),
        ("E741", "Value is locked: g:n")
    );
    let error = scope
        .set_scoped(ScopeKind::Global, b"l", 0, list(&[2]))
        .unwrap_err();
    assert_eq!(
        (error.code, error.message.as_str()),
        ("E741", "Value is locked: g:l")
    );
    let error = scope.set(b"s", number(6)).unwrap_err();
    assert_eq!(
        (error.code, error.message.as_str()),
        ("E741", "Value is locked: s")
    );

    // A refused assignment leaves the old value in place.
    assert_eq!(
        scope.get_scoped(ScopeKind::Global, b"n", 0).unwrap(),
        &number(1)
    );
    assert_eq!(scope.get(b"s", 0).unwrap(), &number(5));

    scope.unlockvar(b"g:n", 2).unwrap();
    scope
        .set_scoped(ScopeKind::Global, b"n", 0, number(7))
        .unwrap();
    assert_eq!(
        scope.get_scoped(ScopeKind::Global, b"n", 0).unwrap(),
        &number(7)
    );

    // Depth 0 marks the variable without locking its value: the assignment
    // is refused as E1122, and the value itself still accepts add().
    scope.lockvar(b"g:n", 0).unwrap();
    let error = scope
        .set_scoped(ScopeKind::Global, b"n", 0, number(8))
        .unwrap_err();
    assert_eq!(
        (error.code, error.message.as_str()),
        ("E1122", "Variable is locked: g:n")
    );
    scope
        .set_scoped(ScopeKind::Global, b"d", 0, list(&[1]))
        .unwrap();
    scope.lockvar(b"g:d", 0).unwrap();
    let target = scope
        .get_scoped(ScopeKind::Global, b"d", 0)
        .unwrap()
        .clone();
    assert_eq!(call("add", vec![target, number(5)]).unwrap(), list(&[1, 5]));

    // Boundary: a name with no dict item yet carries neither flag, so it
    // assigns; an unqualified name resolves the way `get` does, so the
    // locked `g:n` is what an unqualified `n` would replace.
    assert!(scope.check_assignable(b"fresh", 0).is_ok());
    scope.set(b"fresh", number(1)).unwrap();
    assert_eq!(scope.set(b"n", number(3)).unwrap_err().code, "E1122");
}

// `exists('*name')` is what `check.vim`'s `CheckFunction` is built on, so it
// has to answer for callability, not for the generated inventory. Oracle:
// `f_exists` (`eval/funcs.c:1270`) → `function_exists`.
#[test]
fn exists_star_answers_only_for_builtins_this_port_can_call() {
    for (query, expected) in [
        ("*strlen", 1),
        ("*printf", 1),
        ("*setqflist", 0),
        ("*foldtextresult", 0),
        ("*timer_start", 0),
        ("*DefinitelyMissing", 0),
    ] {
        assert_eq!(
            call("exists", vec![text(query)]).unwrap(),
            number(expected),
            "exists('{query}')"
        );
    }

    // Not tautological: the three zeroes above are all *in* the generated
    // table, so the 0 comes from the dispatch arm being absent and not from
    // the name being unknown. `*DefinitelyMissing` is the unknown-name case.
    for name in ["setqflist", "foldtextresult", "timer_start"] {
        assert!(
            crate::builtins::builtin_spec(name).is_some(),
            "{name} left the generated table"
        );
        assert!(
            matches!(
                call(name, vec![]).unwrap_err().kind,
                EvalErrorKind::NotImplemented(_)
            ),
            "{name}"
        );
    }
    assert!(crate::builtins::builtin_spec("DefinitelyMissing").is_none());
}

// Guards the other direction: a name this predicate claims must reach a real
// dispatch arm, because the arm-less fallthrough is also `NotImplemented`
// (`builtins.rs` `dispatch`) and `exists()` would then over-claim again.
#[test]
fn every_builtin_claimed_implemented_reaches_a_dispatch_arm() {
    for spec in BUILTINS {
        if !crate::builtins::is_builtin_implemented(spec.name) {
            continue;
        }
        // A Dict argument is refused by every conversion, so no builtin here
        // does any work; only the arm lookup is exercised.
        let args = vec![Typval::dict(vec![]); spec.min_args];
        if let Err(error) = call(spec.name, args) {
            assert!(
                !matches!(error.kind, EvalErrorKind::NotImplemented(_)),
                "{} is claimed implemented but has no dispatch arm",
                spec.name,
            );
        }
    }
}

// `TYPVAL_ENCODE_CONV_FLOAT` (`eval/encode.c:351`): `%g` through
// `vim_snprintf`, which keeps the zero after the dot, and a re-readable
// `str2float()` call for the two values `%g` cannot round-trip.
#[test]
fn string_renders_floats_the_way_upstream_encodes_them() {
    for (value, expected) in [
        (1.0, "1.0"),
        (0.0, "0.0"),
        (-0.0, "-0.0"),
        (1.23, "1.23"),
        (-1.23, "-1.23"),
        (9_999_999.9, "9999999.9"),
        (1.0e20, "1.0e20"),
        (1.0e-5, "1.0e-5"),
        (0.001, "0.001"),
        (f64::INFINITY, "str2float('inf')"),
        (f64::NEG_INFINITY, "-str2float('inf')"),
        (f64::NAN, "str2float('nan')"),
    ] {
        assert_eq!(
            call("string", vec![Typval::Float(value)]).unwrap(),
            text(expected),
            "string({value})"
        );
    }
    // A Float inside a container uses the same rendering.
    assert_eq!(
        call(
            "string",
            vec![Typval::list(vec![
                Typval::Float(1.0),
                Typval::Float(f64::INFINITY)
            ])]
        )
        .unwrap(),
        text("[1.0, str2float('inf')]"),
    );
}

// `tv_get_string_buf_chk` (`typval.c:4684-4685`) renders `VAR_FLOAT` with
// `%g` and never errors, so every builtin that wants a String takes a Float.
// Each expectation below was measured on the oracle (v0.13.0-dev-1390).
#[test]
fn builtins_coerce_a_float_to_its_percent_g_rendering() {
    assert_eq!(call("strlen", vec![Typval::Float(1.0)]).unwrap(), number(3));
    assert_eq!(
        call("strlen", vec![Typval::Float(1.0e20)]).unwrap(),
        number(6)
    );
    assert_eq!(
        call("strchars", vec![Typval::Float(1.0)]).unwrap(),
        number(3)
    );
    assert_eq!(
        call("toupper", vec![Typval::Float(1.5)]).unwrap(),
        text("1.5")
    );
    assert_eq!(call("str2nr", vec![Typval::Float(1.5)]).unwrap(), number(1));
    assert_eq!(
        call("strlen", vec![Typval::Float(f64::INFINITY)]).unwrap(),
        number(3)
    );
    assert_eq!(
        call("strlen", vec![Typval::Float(f64::NAN)]).unwrap(),
        number(3)
    );

    // `f_len` (`funcs.c:3813-3816`) refuses a Float with E701, so the
    // rendering must not leak into `len()` as the answer 3.
    assert_eq!(
        call("len", vec![Typval::Float(1.0)]).unwrap_err().code,
        "E701"
    );
    // The other E701 arms, which `len` shared with `strlen` before the split.
    assert_eq!(
        call("len", vec![Typval::Bool(true)]).unwrap_err().code,
        "E701"
    );
    assert_eq!(
        call("len", vec![Typval::Special(ox_types::Special::Null)])
            .unwrap_err()
            .code,
        "E701"
    );
    assert_eq!(call("len", vec![funcref("abs")]).unwrap_err().code, "E701");
    // `strlen` coerces where `len` refuses, and raises the String errors.
    assert_eq!(call("strlen", vec![Typval::Bool(true)]).unwrap(), number(6));
    assert_eq!(
        call("strlen", vec![Typval::Special(ox_types::Special::Null)]).unwrap(),
        number(6)
    );
    assert_eq!(
        call("strlen", vec![list(&[1, 2])]).unwrap_err().code,
        "E730"
    );
    assert_eq!(
        call("strlen", vec![Typval::Blob(vec![0x10, 0x20])])
            .unwrap_err()
            .code,
        "E976"
    );
    assert_eq!(
        call("strlen", vec![funcref("abs")]).unwrap_err().code,
        "E729"
    );
    // Both still answer for the shapes they always agreed on.
    assert_eq!(call("len", vec![Typval::Number(12)]).unwrap(), number(2));
    assert_eq!(call("strlen", vec![Typval::Number(12)]).unwrap(), number(2));
    assert_eq!(
        call("len", vec![Typval::Blob(vec![0x10, 0x20])]).unwrap(),
        number(2)
    );
    assert_eq!(call("len", vec![list(&[1, 2])]).unwrap(), number(2));
}

// Oracle: `test/old/testdir/test_float_func.vim` `Test_str2float`, plus the
// spellings measured directly on nvim v0.13.0-dev-1390. `f_str2float`
// (`funcs.c:7042-7056`) and `string2float` (`eval.c:4611-4630`) are the spec.
#[test]
#[expect(clippy::float_cmp, reason = "tests assert exact float parsing results")]
fn str2float_parses_what_string2float_parses() {
    for (input, expected) in [
        ("1", 1.0),
        (" 1 ", 1.0),
        (" 1.0 ", 1.0),
        ("1.23", 1.23),
        ("1.23abc", 1.23),
        ("1e40", 1.0e40),
        ("-1.23", -1.23),
        (" + 1.23 ", 1.23),
        ("+1", 1.0),
        (" +1 ", 1.0),
        (" + 1 ", 1.0),
        ("-1", -1.0),
        (" -1 ", -1.0),
        (" - 1 ", -1.0),
        ("+0.0", 0.0),
        ("1e1000", f64::INFINITY),
        // The three spellings `string2float` matches ahead of `strtod`,
        // case-insensitively and as a three-byte prefix.
        ("inf", f64::INFINITY),
        ("-inf", f64::NEG_INFINITY),
        ("+inf", f64::INFINITY),
        ("Inf", f64::INFINITY),
        ("INF", f64::INFINITY),
        ("  +inf  ", f64::INFINITY),
        ("  -inf", f64::NEG_INFINITY),
        ("infinity", f64::INFINITY),
        ("inf3", f64::INFINITY),
        // `strtod`'s own grammar, reached only when none of the three match.
        (".5", 0.5),
        ("12.", 12.0),
        ("1e3", 1000.0),
        ("0x10", 16.0),
        ("0x1p3", 8.0),
        ("0x1.8p1", 3.0),
        ("abc", 0.0),
        ("", 0.0),
        ("  ", 0.0),
        ("1e", 1.0),
        (" + 1e2 ", 100.0),
        ("\t 3.5", 3.5),
    ] {
        let Typval::Float(answer) = call("str2float", vec![text(input)]).unwrap() else {
            panic!("str2float({input:?}) is not a Float");
        };
        assert_eq!(answer, expected, "str2float({input:?})");
    }

    // NaN never compares equal, so these two are checked by shape. `-nan` is
    // still a NaN because upstream multiplies by -1 rather than branching.
    for input in ["nan", "NaN", "  nan  ", "-nan", "nanx"] {
        let Typval::Float(answer) = call("str2float", vec![text(input)]).unwrap() else {
            panic!("str2float({input:?}) is not a Float");
        };
        assert!(answer.is_nan(), "str2float({input:?}) = {answer}");
    }

    // A sign with nothing behind it keeps its sign on the zero, which
    // `string()` shows and `==` does not.
    assert_eq!(
        call(
            "string",
            vec![call("str2float", vec![text("-0.0")]).unwrap()]
        )
        .unwrap(),
        text("-0.0")
    );
    assert_eq!(
        call("string", vec![call("str2float", vec![text("-")]).unwrap()]).unwrap(),
        text("-0.0")
    );
    assert_eq!(
        call(
            "string",
            vec![call("str2float", vec![text("+0.0")]).unwrap()]
        )
        .unwrap(),
        text("0.0")
    );

    // `Test_str2float`'s type cases: a Float argument coerces through
    // `tv_get_string`, and the container types raise their String errors.
    assert_eq!(
        call("str2float", vec![Typval::Float(1.2)]).unwrap(),
        Typval::Float(1.2)
    );
    assert_eq!(
        call("str2float", vec![number(12)]).unwrap(),
        Typval::Float(12.0)
    );
    assert_eq!(call("str2float", vec![list(&[])]).unwrap_err().code, "E730");
    assert_eq!(
        call("str2float", vec![Typval::dict(vec![])])
            .unwrap_err()
            .code,
        "E731"
    );
    assert_eq!(
        call("str2float", vec![funcref("string")]).unwrap_err().code,
        "E729"
    );
}

// `abs()` is the visible symptom and the wrong diagnosis: `f_abs`
// (`funcs.c:424-441`) only takes the Float path for a Float, and hands
// everything else to `tv_get_number_chk`. There is no String-to-Float
// coercion anywhere in upstream — `tv_get_float` (`typval.c:4413-4415`)
// answers E892 for a String and `tv_get_float_chk` (`typval.h:404`) answers
// E808 — so `abs('-12')` is the Number 12 and `sqrt('12')` is an error.
//
// Oracle: `test/old/testdir/test_float_func.vim` `Test_abs`, plus the
// base-detection and sign cases measured on v0.13.0-dev-1390.
#[test]
fn a_string_reaches_a_number_context_through_vim_str2nr() {
    // `Test_abs`: the answer is a Number, not a Float, and keeps the prefix.
    assert_eq!(call("abs", vec![text("-12")]).unwrap(), number(12));
    assert_eq!(call("abs", vec![text("12abc")]).unwrap(), number(12));
    assert_eq!(call("abs", vec![text("-12abc")]).unwrap(), number(12));
    assert_eq!(call("abs", vec![text("abc")]).unwrap(), number(0));
    assert_eq!(call("abs", vec![text("")]).unwrap(), number(0));
    assert_eq!(call("abs", vec![list(&[])]).unwrap_err().code, "E745");
    assert_eq!(
        call("abs", vec![Typval::dict(vec![])]).unwrap_err().code,
        "E728"
    );
    assert_eq!(
        call("abs", vec![funcref("string")]).unwrap_err().code,
        "E703"
    );
    assert_eq!(
        call("abs", vec![Typval::Blob(vec![0x10])])
            .unwrap_err()
            .code,
        "E974"
    );
    // `-n` is a plain negation, so `VARNUMBER_MIN` negates back to itself.
    assert_eq!(
        call("abs", vec![text("-9223372036854775808")]).unwrap(),
        number(i64::MIN)
    );
    assert_eq!(
        call("abs", vec![number(i64::MIN)]).unwrap(),
        number(i64::MIN)
    );
    // The Float path is untouched and still answers with a Float.
    assert_eq!(
        call("abs", vec![Typval::Float(-1.23)]).unwrap(),
        Typval::Float(1.23)
    );

    // No String-to-Float coercion: a float context refuses a String outright.
    assert_eq!(call("sqrt", vec![text("12")]).unwrap_err().code, "E808");
    assert_eq!(call("cos", vec![text("a")]).unwrap_err().code, "E808");
    assert_eq!(call("float2nr", vec![text("12")]).unwrap_err().code, "E808");

    // `vim_str2nr(…, STR2NR_ALL, …)`: base detection, and the three rules a
    // decimal-only prefix scan gets wrong.
    for (input, expected) in [
        ("0x10", 16),
        ("0X1f", 31),
        ("0b11", 3),
        ("0o17", 15),
        // A leading zero is octal only while every digit stays octal.
        ("010", 8),
        ("08", 8),
        ("019", 19),
        ("0", 0),
        // No white space is skipped, and a `+` is not a sign.
        (" 12", 0),
        (" -12 ", 0),
        ("+12", 0),
        ("-12", -12),
        ("12 ", 12),
        ("-", 0),
        // The accumulator is unsigned and saturates at both ends: `2^63`
        // fits in it and only clamps on the way to a signed Number, while
        // 23 nines overflow `UVARNUMBER_MAX` inside the digit loop.
        ("-9223372036854775808", i64::MIN),
        ("9223372036854775808", i64::MAX),
        ("18446744073709551616", i64::MAX),
        ("99999999999999999999999", i64::MAX),
        ("-99999999999999999999999", i64::MIN),
        ("0xffffffffffffffffff", i64::MAX),
    ] {
        assert_eq!(
            call("and", vec![text(input), number(-1)]).unwrap(),
            number(expected),
            "str2nr-all({input:?})"
        );
    }
}

// Oracle: `test/old/testdir/test_float_func.vim` `Test_float2nr`, whose
// `max_number`/`min_number` are `1/0` and `-(1/0)`, so it asks for
// ±VARNUMBER_MAX at the bounds and `min_number/2-1` just inside the low one.
// Every value below was also measured directly on v0.13.0-dev-1390.
#[test]
fn float2nr_saturates_at_plus_and_minus_varnumber_max() {
    // The boundary is ±2^63, and it saturates to ±VARNUMBER_MAX — the low end
    // is -9223372036854775807, one short of `i64::MIN`.
    for (value, expected) in [
        (1.234, 1),
        (1.234e2, 123),
        (123.4e-1, 12),
        (-1.5, -1),
        (-0.5, 0),
        (0.0, 0),
        (-0.0, 0),
        // `Test_float2nr`'s `pow(2, 62)` / `pow(2, 63)` / `pow(2, 64)` rows.
        (4_611_686_018_427_387_904.0, 4_611_686_018_427_387_904),
        (9_223_372_036_854_775_808.0, i64::MAX),
        (18_446_744_073_709_551_616.0, i64::MAX),
        (-4_611_686_018_427_387_904.0, -4_611_686_018_427_387_904),
        (-9_223_372_036_854_775_808.0, -i64::MAX),
        (-18_446_744_073_709_551_616.0, -i64::MAX),
        (1.0e19, i64::MAX),
        (-1.0e19, -i64::MAX),
        (f64::INFINITY, i64::MAX),
        (f64::NEG_INFINITY, -i64::MAX),
        // The largest magnitudes strictly inside ±2^63 are exact, so they
        // pass the bounds and come out of the cast unchanged. This is the
        // pair that pins the boundary itself rather than just the clamp.
        (9_223_372_036_854_774_784.0, 9_223_372_036_854_774_784),
        (-9_223_372_036_854_774_784.0, -9_223_372_036_854_774_784),
        // NaN fails both comparisons and reaches the cast.
        (f64::NAN, i64::MIN),
    ] {
        assert_eq!(
            call("float2nr", vec![Typval::Float(value)]).unwrap(),
            number(expected),
            "float2nr({value})"
        );
    }

    // `trunc` is a Float function and no longer shares the arm above.
    for (value, expected) in [
        (2.1, 2.0),
        (2.5, 2.0),
        (2.9, 2.0),
        (-2.1, -2.0),
        (-2.9, -2.0),
    ] {
        assert_eq!(
            call("trunc", vec![Typval::Float(value)]).unwrap(),
            Typval::Float(expected),
            "trunc({value})"
        );
    }
    assert_eq!(call("trunc", vec![number(4)]).unwrap(), Typval::Float(4.0));
    assert_eq!(
        call(
            "string",
            vec![call("trunc", vec![Typval::Float(2.1)]).unwrap()]
        )
        .unwrap(),
        text("2.0")
    );
    assert_eq!(call("trunc", vec![text("")]).unwrap_err().code, "E808");
}

// Oracle: `test/old/testdir/test_expr.vim` `Test_printf_float`, which is the
// spec for `vim_snprintf`'s float conversions (`strings.c:2075-2196`).
#[test]
fn printf_float_conversions_match_vim_snprintf() {
    let inf = Typval::Float(f64::INFINITY);
    let neg_inf = Typval::Float(f64::NEG_INFINITY);
    let nan = Typval::Float(f64::NAN);
    let third = Typval::Float(1.0 / 3.0);
    let neg_third = Typval::Float(-1.0 / 3.0);
    for (format, value, expected) in [
        ("%f", Typval::Number(1), "1.000000"),
        ("%f", Typval::Float(1.23), "1.230000"),
        ("%F", Typval::Float(1.23), "1.230000"),
        ("%g", Typval::Float(9_999_999.9), "9999999.9"),
        ("%G", Typval::Float(9_999_999.9), "9999999.9"),
        ("%.8g", Typval::Float(10_000_000.1), "1.00000001e7"),
        ("%.8G", Typval::Float(10_000_000.1), "1.00000001E7"),
        ("%e", Typval::Float(1.23), "1.230000e+00"),
        ("%E", Typval::Float(1.23), "1.230000E+00"),
        ("%e", Typval::Float(0.012), "1.200000e-02"),
        ("%e", Typval::Float(-0.012), "-1.200000e-02"),
        ("%.2f", third.clone(), "0.33"),
        ("%6.2f", third.clone(), "  0.33"),
        ("%6.2f", neg_third.clone(), " -0.33"),
        ("%06.2f", third.clone(), "000.33"),
        ("%06.2f", neg_third.clone(), "-00.33"),
        ("%+06.2f", neg_third.clone(), "-00.33"),
        ("%+06.2f", third.clone(), "+00.33"),
        ("% 06.2f", third.clone(), " 00.33"),
        ("%06.2g", third.clone(), "000.33"),
        ("%06.2g", neg_third.clone(), "-00.33"),
        ("%3.2f", third.clone(), "0.33"),
        ("%010.2e", third.clone(), "003.33e-01"),
        ("% 010.2e", third.clone(), " 03.33e-01"),
        ("%+010.2e", third.clone(), "+03.33e-01"),
        ("%010.2e", neg_third, "-03.33e-01"),
        // Precision 0 drops the dot.
        ("%3.f", Typval::Float(7.0 / 3.0), "  2"),
        ("%3.g", Typval::Float(7.0 / 3.0), "  2"),
        ("%7.e", Typval::Float(7.0 / 3.0), "  2e+00"),
        // Zero can be signed; infinity can be signed; NaN never is.
        ("%+f", Typval::Float(0.0), "+0.000000"),
        ("%f", Typval::Float(0.0), "0.000000"),
        ("%f", Typval::Float(-0.0), "-0.000000"),
        ("%s", Typval::Float(0.0), "0.0"),
        ("%s", Typval::Float(-0.0), "-0.0"),
        ("%f", inf.clone(), "inf"),
        ("%f", neg_inf.clone(), "-inf"),
        ("%g", inf.clone(), "inf"),
        ("%e", neg_inf.clone(), "-inf"),
        ("%F", inf.clone(), "INF"),
        ("%E", neg_inf.clone(), "-INF"),
        ("%G", neg_inf.clone(), "-INF"),
        ("%+f", inf.clone(), "+inf"),
        ("% f", inf.clone(), " inf"),
        ("%6f", inf.clone(), "   inf"),
        ("%6f", neg_inf.clone(), "  -inf"),
        ("%+06f", inf.clone(), "  +inf"),
        ("%-6f", inf.clone(), "inf   "),
        ("%-+6f", inf.clone(), "+inf  "),
        ("%- 6f", inf.clone(), " inf  "),
        ("%-6G", neg_inf.clone(), "-INF  "),
        ("%s", inf.clone(), "str2float('inf')"),
        ("%s", neg_inf, "-str2float('inf')"),
        ("%f", nan.clone(), "nan"),
        ("%g", nan.clone(), "nan"),
        ("%F", nan.clone(), "NAN"),
        ("%E", nan.clone(), "NAN"),
        ("%6f", nan.clone(), "   nan"),
        ("%06f", nan.clone(), "   nan"),
        ("%-6f", nan.clone(), "nan   "),
        ("%s", nan, "str2float('nan')"),
    ] {
        assert_eq!(
            call("printf", vec![text(format), value]).unwrap(),
            text(expected),
            "printf('{format}', …)",
        );
    }

    // `%.330f` prints 330 decimals; the precision is capped at `TMP_LEN - 10`
    // (`strings.c:2123`), so `%.340f` and `%.350f` both print 340.
    for (precision, decimals) in [(330usize, 330usize), (340, 340), (350, 340)] {
        let rendered = call(
            "printf",
            vec![text(&format!("%.{precision}f")), Typval::Float(1.0)],
        )
        .unwrap();
        assert_eq!(
            rendered,
            text(&format!("1.{}", "0".repeat(decimals))),
            "%.{precision}f"
        );
    }

    // `tv_float` (`strings.c:716`) has its own error for a non-numeric value.
    assert_eq!(
        call("printf", vec![text("%f"), text("a")])
            .unwrap_err()
            .code,
        "E807"
    );
}

// One case per builtin added here, from the `eval.lua` doc examples and
// `test/old/testdir/test_float_func.vim`.
#[test]
fn float_builtins_answer_like_libm() {
    let one = |name: &str, argument: f64| match call(name, vec![Typval::Float(argument)]).unwrap() {
        Typval::Float(value) => value,
        other => panic!("{name} returned {other:?}"),
    };
    let close = |value: f64, expected: f64| {
        assert!((value - expected).abs() < 1.0e-12, "{value} != {expected}");
    };

    close(one("acos", 0.0), std::f64::consts::FRAC_PI_2);
    close(one("asin", 1.0), std::f64::consts::FRAC_PI_2);
    close(one("atan", 1.0), std::f64::consts::FRAC_PI_4);
    close(one("cos", 0.0), 1.0);
    close(one("cosh", 0.0), 1.0);
    close(one("exp", 1.0), std::f64::consts::E);
    close(one("log", std::f64::consts::E), 1.0);
    close(one("log10", 1000.0), 3.0);
    close(one("sin", 0.0), 0.0);
    close(one("sinh", 0.0), 0.0);
    close(one("tan", 0.0), 0.0);
    close(one("tanh", 0.0), 0.0);
    // `round()` is half-away-from-zero, unlike `floor(x + 0.5)`.
    assert_eq!(
        call("round", vec![Typval::Float(0.456)]).unwrap(),
        Typval::Float(0.0)
    );
    assert_eq!(
        call("round", vec![Typval::Float(4.5)]).unwrap(),
        Typval::Float(5.0)
    );
    assert_eq!(
        call("round", vec![Typval::Float(-4.5)]).unwrap(),
        Typval::Float(-5.0)
    );
    close(
        match call("atan2", vec![Typval::Float(-1.0), Typval::Float(1.0)]).unwrap() {
            Typval::Float(value) => value,
            other => panic!("atan2 returned {other:?}"),
        },
        -std::f64::consts::FRAC_PI_4,
    );
    assert_eq!(
        call("fmod", vec![Typval::Float(12.33), Typval::Float(1.22)]).unwrap(),
        Typval::Float(12.33_f64 % 1.22)
    );

    // `f_isinf`/`f_isnan` (`funcs.c:3141-3154`) answer only for a Float: a
    // Number never carries an infinity, so it is 0 rather than an error.
    assert_eq!(
        call("isinf", vec![Typval::Float(f64::INFINITY)]).unwrap(),
        number(1)
    );
    assert_eq!(
        call("isinf", vec![Typval::Float(f64::NEG_INFINITY)]).unwrap(),
        number(-1)
    );
    assert_eq!(call("isinf", vec![Typval::Float(1.0)]).unwrap(), number(0));
    assert_eq!(call("isinf", vec![number(1)]).unwrap(), number(0));
    assert_eq!(call("isinf", vec![text("inf")]).unwrap(), number(0));
    assert_eq!(
        call("isnan", vec![Typval::Float(f64::NAN)]).unwrap(),
        number(1)
    );
    assert_eq!(call("isnan", vec![Typval::Float(0.0)]).unwrap(), number(0));
    assert_eq!(call("isnan", vec![number(0)]).unwrap(), number(0));

    // The unary family shares `float_op_wrapper`'s conversion, so a String
    // argument is E808 the way `sqrt("a")` already is.
    assert_eq!(call("cos", vec![text("a")]).unwrap_err().code, "E808");
}

/// `f_getenv` (`eval/funcs.c:1104-1115`) and `environ()`
/// (`runtime/lua/vim/_core/vimfn.lua:16-26`).
///
/// Oracle, `nvim --headless -u <lua>` in a sandbox with `HOME` set:
/// `vim.fn.getenv('HOME')` is the path, `vim.fn.getenv('T78_NOPE')` is
/// `vim.NIL`, `type(vim.fn.environ())` is `table` and
/// `vim.fn.environ()['HOME']` is the same path. The `v:null` answer, not an
/// empty string, is the whole point: it is how a caller tells unset from set
/// to empty, and `plenary/log.lua:12` is where telescope.nvim needs it.
#[test]
fn getenv_and_environ_answer_the_process_environment() {
    const NAME: &str = "OXVIM_TEST_EVAL_GETENV";
    const MISSING: &str = "OXVIM_TEST_EVAL_GETENV_UNSET";
    let _guards = (EnvGuard::take(NAME), EnvGuard::take(MISSING));
    let mut builtins = Builtins::without_regex();
    let mut scope = Scope::new();
    let call = |builtins: &mut Builtins<'_>, scope: &mut Scope, name: &str, args: Vec<Typval>| {
        builtins.call(&OxStr::from(name), args, scope)
    };

    ox_sys::unset_env(MISSING);
    assert_eq!(
        call(&mut builtins, &mut scope, "getenv", vec![text(MISSING)]).unwrap(),
        Typval::Special(Special::Null),
    );

    // Through `setenv`, so the value is in the process environment `getenv`
    // reads; there is no other copy to keep in step.
    call(
        &mut builtins,
        &mut scope,
        "setenv",
        vec![text(NAME), text("value")],
    )
    .unwrap();
    assert_eq!(
        call(&mut builtins, &mut scope, "getenv", vec![text(NAME)]).unwrap(),
        text("value")
    );

    let environment = call(&mut builtins, &mut scope, "environ", Vec::new()).unwrap();
    let Typval::Dict(entries) = &environment else {
        panic!("environ() must answer a Dict: {environment:?}")
    };
    let entries = entries.borrow().entries.clone();
    assert_eq!(
        entries
            .iter()
            .find(|entry| entry.key == OxStr::from(NAME))
            .map(|entry| entry.value.clone()),
        Some(text("value")),
    );
    assert!(
        !entries
            .iter()
            .any(|entry| entry.key == OxStr::from(MISSING))
    );

    call(
        &mut builtins,
        &mut scope,
        "setenv",
        vec![text(NAME), Typval::Special(Special::Null)],
    )
    .unwrap();
    assert_eq!(
        call(&mut builtins, &mut scope, "getenv", vec![text(NAME)]).unwrap(),
        Typval::Special(Special::Null),
    );
}

/// `f_fnameescape` (`eval/funcs.c:1517-1521`) through
/// `vim_strsave_fnameescape(fname, VSE_NONE)`, whose escape set is
/// `PATH_ESC_CHARS` at `ex_getln.c:4103` and whose leading-character special
/// case is at `ex_getln.c:4118-4122`.
///
/// Oracle: `vim.fn.fnameescape('a b|c%d#e*f[g')` is `a\ b\|c\%d\#e\*f\[g`,
/// `fnameescape('>x')` is `\>x`, `fnameescape('-')` is `\-`.
#[test]
fn fnameescape_escapes_the_path_character_set() {
    assert_eq!(
        call("fnameescape", vec![text("a b|c%d#e*f[g")]).unwrap(),
        text("a\\ b\\|c\\%d\\#e\\*f\\[g"),
    );
    // `>` and `+` lead some Ex commands and `cd -` has its own meaning.
    assert_eq!(call("fnameescape", vec![text(">x")]).unwrap(), text("\\>x"));
    assert_eq!(call("fnameescape", vec![text("+x")]).unwrap(), text("\\+x"));
    assert_eq!(call("fnameescape", vec![text("-")]).unwrap(), text("\\-"));
    // Only a lone `-`: upstream tests `p[1] == NUL`.
    assert_eq!(call("fnameescape", vec![text("-x")]).unwrap(), text("-x"));
    // `]`, `}` and `&` are not in PATH_ESC_CHARS, unlike SHELL_ESC_CHARS.
    assert_eq!(
        call("fnameescape", vec![text("a]b}c&d")]).unwrap(),
        text("a]b}c&d")
    );
}

/// `f_localtime` (`eval/funcs.c:3924-3927`), `f_reltime`
/// (`eval/funcs.c:5096-5134`), `f_reltimefloat` (`eval/funcs.c:6774-6784`)
/// and `f_reltimestr` (`eval/funcs.c:5138-5148`) through `profile_msg`
/// (`profile.c:72-78`).
///
/// A reltime value is one 64-bit nanosecond count split across two 32-bit
/// list items, high half first (`list2proftime`, `eval/funcs.c:5065-5085`), so
/// the arithmetic is pinned with literal values the oracle agrees on rather
/// than with a clock reading:
/// `reltimefloat(reltime([0,0],[2,500000000]))` is `9.089934592` on the oracle
/// -- `(2 << 32) + 500000000` nanoseconds -- and `reltimestr` of the same
/// value is `"  9.089935"`, right-aligned in ten columns by `"%10.6lf"`.
#[test]
fn localtime_and_the_reltime_family_match_the_upstream_encoding() {
    let Typval::Number(seconds) = call("localtime", Vec::new()).unwrap() else {
        panic!("localtime() must answer a Number")
    };
    assert!(seconds > 1_700_000_000, "localtime() answered {seconds}");

    let elapsed = call("reltime", vec![list(&[0, 0]), list(&[2, 500_000_000])]).unwrap();
    assert_eq!(elapsed, list(&[2, 500_000_000]));
    assert_eq!(
        call("reltimefloat", vec![elapsed.clone()]).unwrap(),
        Typval::Float(9.089_934_592)
    );
    assert_eq!(
        call("reltimestr", vec![elapsed]).unwrap(),
        text("  9.089935")
    );

    // A difference small enough to stay inside the low half stays there.
    assert_eq!(
        call("reltime", vec![list(&[0, 0]), list(&[0, 1000])]).unwrap(),
        list(&[0, 1000])
    );

    // No argument reads the clock, and one argument is the time since it, so
    // the elapsed value is non-negative and the list shape is always two.
    let start = call("reltime", Vec::new()).unwrap();
    let Typval::List(items) = &start else {
        panic!("reltime() must answer a List")
    };
    assert_eq!(items.borrow().items.len(), 2);
    let Typval::Float(since) =
        call("reltimefloat", vec![call("reltime", vec![start]).unwrap()]).unwrap()
    else {
        panic!("reltimefloat() must answer a Float")
    };
    assert!(since >= 0.0, "elapsed time went backwards: {since}");
}

/// UAX 29 extended grapheme clusters: VS, ZWJ, combining marks, and regional
/// indicator pairs all travel with their base. `strchars(..., 0)` stays a
/// Cluster counting agrees across `strchars(..., 1)` and `strcharlen` for
/// one codepoint-count versus cluster-count pair per shape.
#[test]
fn composed_character_counts_agree_on_uax29_clusters() {
    // (input, codepoints, clusters): heart with variation selector,
    // transgender flag, farmer ZWJ, combining-mark run, and a base plus
    // cluster followed by a second base.
    let cases = [
        ("\u{2764}\u{fe0f}", 2, 1),
        ("\u{1f3f3}\u{fe0f}\u{200d}\u{26a7}\u{fe0f}", 5, 1),
        ("\u{1f9d1}\u{200d}\u{1f33e}", 3, 1),
        ("e\u{0301}\u{0308}", 3, 1),
        ("e\u{0301}x", 3, 2),
        ("\r\n", 2, 2),
    ];
    for (input, codepoints, clusters) in cases {
        assert_eq!(
            call("strchars", vec![text(input)]).unwrap(),
            number(codepoints),
            "{input:?} codepoints"
        );
        assert_eq!(
            call("strchars", vec![text(input), number(1)]).unwrap(),
            number(clusters),
            "{input:?} clusters"
        );
        assert_eq!(
            call("strcharlen", vec![text(input)]).unwrap(),
            number(clusters),
            "{input:?} strcharlen"
        );
    }
}

/// Cluster boundaries drive `strcharpart(..., skipcc)` selection and
/// `byteidx` offsets; both consume the same UAX 29 segmentation.
#[test]
fn composed_character_boundaries_drive_slicing_apis() {
    // Combining-mark run and a two-cluster string share the fixture shapes.
    let combining = "e\u{0301}\u{0308}";
    let two_clusters = "e\u{0301}x";

    // strcharpart with skipcc selects one cluster at a time.
    assert_eq!(
        call(
            "strcharpart",
            vec![text(combining), number(0), number(1), number(1)]
        )
        .unwrap(),
        text("e\u{0301}\u{0308}")
    );
    // Second cluster from the two-cluster string.
    assert_eq!(
        call(
            "strcharpart",
            vec![text(two_clusters), number(1), number(1), number(1)]
        )
        .unwrap(),
        text("x")
    );

    // byteidx at the next cluster boundary: 'e' is 1 byte, U+0301 is 2 bytes,
    // so cluster 0 ends at byte 3 and cluster 1 starts at 3.
    assert_eq!(
        call("byteidx", vec![text(two_clusters), number(1)]).unwrap(),
        number(3)
    );
    // byteidx 0 is the start of the first cluster.
    assert_eq!(
        call("byteidx", vec![text(two_clusters), number(0)]).unwrap(),
        number(0)
    );

    // Heart + farmer: two clusters, byteidx at the second.
    let heart_farmer = "\u{2764}\u{fe0f}\u{1f9d1}\u{200d}\u{1f33e}";
    assert_eq!(
        call("strcharlen", vec![text(heart_farmer)]).unwrap(),
        number(2)
    );
    assert_eq!(
        call("byteidx", vec![text(heart_farmer), number(1)]).unwrap(),
        number(6) // heart = 3 + 3 = 6 bytes
    );
}

/// Invalid UTF-8 rides the same composed boundary through real builtin
/// dispatch: every raw byte of an invalid sequence advances exactly one
/// character (`composed_character_len` consumes one byte when the first
/// chunk has no valid prefix). Raw `OxStr` bytes carry inputs `text()`
/// cannot express; a zero-progress regression here hangs the counting
/// loops and misplaces the byte boundaries instead of passing quietly.
#[test]
fn invalid_utf8_rides_composed_character_boundaries() {
    // (bytes, characters, byteidx offsets, per-character slices): an invalid
    // lead before ASCII, a truncated multibyte lead, and an invalid byte
    // between ASCII bytes.
    let invalid_lead = Typval::String(OxStr(b"\xffa".to_vec()));
    assert_eq!(
        call("strcharlen", vec![invalid_lead.clone()]).unwrap(),
        number(2)
    );
    assert_eq!(
        call("strchars", vec![invalid_lead.clone(), number(1)]).unwrap(),
        number(2)
    );
    assert_eq!(
        call("byteidx", vec![invalid_lead.clone(), number(1)]).unwrap(),
        number(1)
    );
    assert_eq!(
        call(
            "strcharpart",
            vec![invalid_lead, number(0), number(1), number(1)]
        )
        .unwrap(),
        Typval::String(OxStr(vec![0xff]))
    );

    // Truncated multibyte lead: the 0xe2 lead loses its final continuation
    // byte, so both raw bytes count one character each before the ASCII tail.
    let truncated_lead = Typval::String(OxStr(b"\xe2\x94a".to_vec()));
    assert_eq!(
        call("strcharlen", vec![truncated_lead.clone()]).unwrap(),
        number(3)
    );
    assert_eq!(
        call("strchars", vec![truncated_lead.clone(), number(1)]).unwrap(),
        number(3)
    );
    assert_eq!(
        call("byteidx", vec![truncated_lead.clone(), number(2)]).unwrap(),
        number(2)
    );
    assert_eq!(
        call(
            "strcharpart",
            vec![truncated_lead, number(1), number(1), number(1)]
        )
        .unwrap(),
        Typval::String(OxStr(vec![0x94]))
    );

    // An invalid byte between ASCII bytes: "a", 0xff, "b" is three characters.
    let invalid_middle = Typval::String(OxStr(b"a\xffb".to_vec()));
    assert_eq!(
        call("strcharlen", vec![invalid_middle.clone()]).unwrap(),
        number(3)
    );
    assert_eq!(
        call("strchars", vec![invalid_middle.clone(), number(1)]).unwrap(),
        number(3)
    );
    assert_eq!(
        call("byteidx", vec![invalid_middle.clone(), number(2)]).unwrap(),
        number(2)
    );
    assert_eq!(
        call(
            "strcharpart",
            vec![invalid_middle, number(1), number(1), number(1)]
        )
        .unwrap(),
        Typval::String(OxStr(vec![0xff]))
    );
}
