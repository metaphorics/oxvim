#![allow(clippy::unwrap_used)]

use crate::script::{RealFileIO, ScriptCtx};

#[test]
fn push_alias_source_reports_sid_without_allocating() {
    let mut ctx = ScriptCtx::new(RealFileIO);
    let next_before = ctx.allocate_sid("other.vim");
    let sid = 42;
    ctx.push_alias_source(sid, 7, 1, "command Hit".to_owned());
    assert_eq!(ctx.current_sid(), Some(sid));
    assert_eq!(ctx.current_seq(), 7);
    let next_after = ctx.allocate_sid("another.vim");
    assert_eq!(next_after, next_before + 1);
}

#[test]
fn push_alias_source_preserves_quoted_snr_expansion() {
    let mut ctx = ScriptCtx::new(RealFileIO);
    ctx.push_alias_source(8, 1, 1, "command Hit".to_owned());
    let command = r#"let g:name = '<SNR>''tail' | call <SNR>Func()"#;
    assert_eq!(
        ctx.expand_snr(command, 8),
        r#"let g:name = '<SNR>''tail' | call <SNR>8_Func()"#
    );
}

/// An apostrophe that is English text inside a double-quoted string, or an
/// Ex mark address, is data. Treating either as a literal opener would
/// suppress every later `<SNR>` on the line and send the call to function
/// lookup with an unresolved name.
#[test]
fn push_alias_source_expands_snr_after_an_unpaired_apostrophe() {
    let mut ctx = ScriptCtx::new(RealFileIO);
    ctx.push_alias_source(8, 1, 1, "command Hit".to_owned());
    for (line, expected) in [
        (
            r#"echo "don't" | call <SNR>Func()"#,
            r#"echo "don't" | call <SNR>8_Func()"#,
        ),
        ("'.call <SNR>Func()", "'.call <SNR>8_Func()"),
        (
            r#"echo "it's" . '<SNR>kept' | call <SNR>Func()"#,
            r#"echo "it's" . '<SNR>kept' | call <SNR>8_Func()"#,
        ),
    ] {
        assert_eq!(ctx.expand_snr(line, 8), expected, "line: {line}");
    }
}
