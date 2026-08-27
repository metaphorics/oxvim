//! Shared, option-aware indentation primitives for operators and open lines.

use std::cmp::Ordering;

use ox_types::BufHandle;
use thiserror::Error;

use crate::{Editor, OptionValue};

const DEFAULT_CINKEYS: &str = "0{,0},0),0],:,0#,!^F,o,O,e";
const DEFAULT_CINWORDS: &str = "if,else,while,do,for,switch";
const DEFAULT_COMMENTS: &str = "s1:/*,mb:*,ex:*/,://,b:#,:%,:XCOMM,n:>,fb:-,fb:•";
const DEFAULT_LISPWORDS: &str = "defun,define,defmacro,set!,lambda,if,case,let,flet,let*,letrec,do,do*,define-syntax,let-syntax,letrec-syntax,destructuring-bind,defpackage,defparameter,defstruct,deftype,defvar,do-all-symbols,do-external-symbols,do-symbols,dolist,dotimes,ecase,etypecase,eval-when,labels,macrolet,multiple-value-bind,multiple-value-call,multiple-value-prog1,multiple-value-setq,prog1,progv,typecase,unless,unwind-protect,when,with-input-from-string,with-open-file,with-open-stream,with-output-to-string,with-package-iterator,define-condition,handler-bind,handler-case,restart-bind,restart-case,with-simple-restart,store-value,use-value,muffle-warning,abort,continue,with-slots,with-slots*,with-accessors,with-accessors*,defclass,defmethod,print-unreadable-object";

/// All options that can affect one indentation decision.
#[derive(Clone, Debug)]
pub(crate) struct IndentOptions {
    pub(crate) shiftwidth: usize,
    pub(crate) tabstop: usize,
    pub(crate) expandtab: bool,
    pub(crate) preserveindent: bool,
    pub(crate) copyindent: bool,
    pub(crate) autoindent: bool,
    pub(crate) smartindent: bool,
    pub(crate) cindent: bool,
    pub(crate) paste: bool,
    pub(crate) cinoptions: Cino,
    pub(crate) cinkeys: String,
    pub(crate) cinwords: String,
    pub(crate) comments: String,
    pub(crate) indentexpr: String,
    pub(crate) indentkeys: String,
    pub(crate) lisp: bool,
    pub(crate) lispoptions_expr: bool,
    pub(crate) lispwords: String,
}

impl Default for IndentOptions {
    fn default() -> Self {
        let shiftwidth = 8;
        Self {
            shiftwidth,
            tabstop: 8,
            expandtab: false,
            preserveindent: false,
            copyindent: false,
            autoindent: false,
            smartindent: false,
            cindent: false,
            paste: false,
            cinoptions: Cino::parse("", shiftwidth),
            cinkeys: DEFAULT_CINKEYS.to_owned(),
            cinwords: DEFAULT_CINWORDS.to_owned(),
            comments: DEFAULT_COMMENTS.to_owned(),
            indentexpr: String::new(),
            indentkeys: DEFAULT_CINKEYS.to_owned(),
            lisp: false,
            lispoptions_expr: false,
            lispwords: DEFAULT_LISPWORDS.to_owned(),
        }
    }
}

impl IndentOptions {
    /// Captures one coherent option snapshot. `shiftwidth=0` resolves to
    /// `tabstop`, matching `get_sw_value()`.
    #[must_use]
    pub(crate) fn capture(editor: &Editor, buffer: BufHandle) -> Self {
        let tabstop = buffer_number(editor, buffer, "tabstop", 8).max(1);
        let configured_shiftwidth = buffer_number(editor, buffer, "shiftwidth", 8);
        let shiftwidth = if configured_shiftwidth == 0 {
            tabstop
        } else {
            configured_shiftwidth.max(1)
        };
        let cinoptions = buffer_string(editor, buffer, "cinoptions", "");
        Self {
            shiftwidth,
            tabstop,
            expandtab: buffer_bool(editor, buffer, "expandtab"),
            preserveindent: buffer_bool(editor, buffer, "preserveindent"),
            copyindent: buffer_bool(editor, buffer, "copyindent"),
            autoindent: buffer_bool(editor, buffer, "autoindent"),
            smartindent: buffer_bool(editor, buffer, "smartindent"),
            cindent: buffer_bool(editor, buffer, "cindent"),
            paste: global_bool(editor, "paste"),
            cinoptions: Cino::parse(&cinoptions, shiftwidth),
            cinkeys: buffer_string(editor, buffer, "cinkeys", DEFAULT_CINKEYS),
            cinwords: buffer_string(editor, buffer, "cinwords", DEFAULT_CINWORDS),
            comments: buffer_string(editor, buffer, "comments", DEFAULT_COMMENTS),
            indentexpr: buffer_string(editor, buffer, "indentexpr", ""),
            indentkeys: buffer_string(editor, buffer, "indentkeys", DEFAULT_CINKEYS),
            lisp: buffer_bool(editor, buffer, "lisp"),
            lispoptions_expr: buffer_string(editor, buffer, "lispoptions", "")
                .split(',')
                .any(|flag| flag == "expr:1"),
            lispwords: buffer_string(editor, buffer, "lispwords", DEFAULT_LISPWORDS),
        }
    }
}

fn buffer_bool(editor: &Editor, buffer: BufHandle, name: &str) -> bool {
    matches!(editor.options().get_buffer(buffer, name), Ok(OptionValue::Boolean(true)))
}

fn global_bool(editor: &Editor, name: &str) -> bool {
    matches!(editor.options().get_global(name), Ok(OptionValue::Boolean(true)))
}

fn buffer_number(editor: &Editor, buffer: BufHandle, name: &str, fallback: usize) -> usize {
    match editor.options().get_buffer(buffer, name) {
        Ok(OptionValue::Number(value)) => usize::try_from(*value).unwrap_or(fallback),
        _ => fallback,
    }
}

fn buffer_string(editor: &Editor, buffer: BufHandle, name: &str, fallback: &str) -> String {
    match editor.options().get_buffer(buffer, name) {
        Ok(OptionValue::String(value)) => value.clone(),
        _ => fallback.to_owned(),
    }
}

/// Number of leading space and tab bytes.
#[must_use]
pub(crate) fn leading_len(line: &[u8]) -> usize {
    line.iter().take_while(|byte| matches!(byte, b' ' | b'\t')).count()
}

/// Counts an indent in display columns using fixed `tabstop` stops.
#[must_use]
pub(crate) fn indent_columns(line: &[u8], opts: &IndentOptions) -> usize {
    let mut col = 0usize;
    for byte in line.iter().copied() {
        match byte {
            b' ' => col = col.saturating_add(1),
            b'\t' => col = col.saturating_add(tab_padding(col, opts.tabstop)),
            _ => break,
        }
    }
    col
}

fn tab_padding(col: usize, tabstop: usize) -> usize {
    let tabstop = tabstop.max(1);
    tabstop - col % tabstop
}

/// Builds the complete leading whitespace for `target_cols`, matching
/// `set_indent()` without `SIN_INSERT`.
#[must_use]
pub(crate) fn whitespace_for(target_cols: usize, current: &[u8], opts: &IndentOptions) -> Vec<u8> {
    whitespace_with_policy(target_cols, current, opts, opts.preserveindent)
}

fn whitespace_with_policy(target_cols: usize, current: &[u8], opts: &IndentOptions, preserve: bool) -> Vec<u8> {
    let mut todo = target_cols;
    let mut ind_done = 0usize;
    let mut out = Vec::with_capacity(target_cols.saturating_add(opts.tabstop));
    if preserve {
        for &byte in current.iter().take_while(|byte| matches!(**byte, b' ' | b'\t')) {
            if todo == 0 {
                break;
            }
            if byte == b'\t' {
                let pad = tab_padding(ind_done, opts.tabstop);
                if pad > todo {
                    break;
                }
                out.push(byte);
                todo -= pad;
                ind_done += pad;
            } else {
                out.push(byte);
                todo -= 1;
                ind_done += 1;
            }
        }
        if !opts.expandtab {
            let pad = tab_padding(ind_done, opts.tabstop);
            if todo >= pad {
                out.push(b'\t');
                todo -= pad;
                ind_done += pad;
            } else if opts.preserveindent && todo == 0 && ind_done == target_cols && out.last() == Some(&b' ') {
                out.push(b'\t');
            }
        }
    }
    if !opts.expandtab {
        loop {
            let pad = tab_padding(ind_done, opts.tabstop);
            if pad > todo {
                break;
            }
            out.push(b'\t');
            todo -= pad;
            ind_done += pad;
        }
    }
    out.resize(out.len().saturating_add(todo), b' ');
    out
}


fn copied_whitespace_for(target_cols: usize, source: &[u8], opts: &IndentOptions) -> Vec<u8> {
    whitespace_with_policy(target_cols, source, opts, true)
}

fn inserted_whitespace_for(target_cols: usize, opts: &IndentOptions) -> Vec<u8> {
    whitespace_with_policy(target_cols, &[], opts, false)
}

fn newline_whitespace_for(target_cols: usize, source: &[u8], opts: &IndentOptions) -> Vec<u8> {
    if opts.copyindent {
        copied_whitespace_for(target_cols, source, opts)
    } else {
        inserted_whitespace_for(target_cols, opts)
    }
}

/// Indent result. `LeaveAlone` is Neovim's `-1` path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IndentAmount {
    Columns(usize),
    LeaveAlone,
}

/// Stage-A `cinoptions` values. Amounts remain signed until the final clamp.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Cino {
    level: i64,
    case_label: i64,
    case_code: i64,
    continuation: i64,
    unclosed: i64,
    unclosed2: i64,
    comment: i64,
    in_comment: i64,
    maxparen: usize,
    maxcomment: usize,
}

impl Cino {
    #[must_use]
    pub(crate) fn parse(value: &str, shiftwidth: usize) -> Self {
        let sw = i64::try_from(shiftwidth).unwrap_or(i64::MAX);
        let mut parsed = Self {
            level: sw,
            case_label: sw,
            case_code: sw,
            continuation: sw,
            unclosed: sw.saturating_mul(2),
            unclosed2: sw,
            comment: 0,
            in_comment: 3,
            maxparen: 20,
            maxcomment: 70,
        };
        for part in value.split(',').filter(|part| !part.is_empty()) {
            let key = part.as_bytes()[0];
            let amount = parse_cino_amount(&part[1..], sw);
            match key {
                b'>' => parsed.level = amount,
                b':' => parsed.case_label = amount,
                b'=' => parsed.case_code = amount,
                b'+' => parsed.continuation = amount,
                b'(' => parsed.unclosed = amount,
                b'u' => parsed.unclosed2 = amount,
                b'/' => parsed.comment = amount,
                b'c' => parsed.in_comment = amount,
                b')' => parsed.maxparen = usize::try_from(amount.max(0)).unwrap_or(usize::MAX),
                b'*' => parsed.maxcomment = usize::try_from(amount.max(0)).unwrap_or(usize::MAX),
                _ => {}
            }
        }
        parsed
    }
}

fn parse_cino_amount(text: &str, shiftwidth: i64) -> i64 {
    let bytes = text.as_bytes();
    let mut index = 0usize;
    let negative = bytes.get(index) == Some(&b'-');
    if negative {
        index += 1;
    }
    let digits_start = index;
    let mut whole = 0i64;
    while let Some(byte) = bytes.get(index).copied().filter(u8::is_ascii_digit) {
        whole = whole.saturating_mul(10).saturating_add(i64::from(byte - b'0'));
        index += 1;
    }
    let had_whole = index > digits_start;
    let mut fraction = 0i64;
    let mut divider = 1i64;
    let mut had_fraction = false;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        while let Some(byte) = bytes.get(index).copied().filter(u8::is_ascii_digit) {
            fraction = fraction.saturating_mul(10).saturating_add(i64::from(byte - b'0'));
            divider = divider.saturating_mul(10);
            had_fraction = true;
            index += 1;
        }
    }
    let in_shifts = bytes.get(index) == Some(&b's');
    let value = if in_shifts {
        if !had_whole && !had_fraction {
            shiftwidth
        } else {
            whole.saturating_mul(shiftwidth).saturating_add(
                shiftwidth.saturating_mul(fraction).saturating_add(divider / 2) / divider,
            )
        }
    } else {
        whole
    };
    if negative { value.saturating_neg() } else { value }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct Pos {
    line: usize,
    byte: usize,
}

#[derive(Clone, Debug)]
struct RawState {
    start: Pos,
    close: Vec<u8>,
}

#[derive(Clone, Debug)]
struct CLine {
    code: Vec<u8>,
    comment_at_start: Option<Pos>,
    raw_at_start: Option<Pos>,
    brace_at_start: Option<Pos>,
    brace_at_end: Option<Pos>,
    paren_at_start: Option<Pos>,
    paren_at_end: Option<Pos>,
    bracket_at_start: Option<Pos>,
    line_comment_col: Option<usize>,
    raw_active: bool,
}

#[derive(Clone, Debug, Default)]
struct CScan {
    lines: Vec<CLine>,
    brace_pairs: Vec<(Pos, Pos)>,
    paren_pairs: Vec<(Pos, Pos)>,
    bracket_pairs: Vec<(Pos, Pos)>,
}

fn scan_c(lines: &[Vec<u8>], through: usize) -> CScan {
    let mut scan = CScan::default();
    let mut braces = Vec::<Pos>::new();
    let mut parens = Vec::<Pos>::new();
    let mut brackets = Vec::<Pos>::new();
    let mut comment = None::<Pos>;
    let mut raw = None::<RawState>;
    for (row, line) in lines.iter().enumerate().take(through.saturating_add(1)) {
        let comment_at_start = comment;
        let raw_at_start = raw.as_ref().map(|state| state.start);
        let brace_at_start = braces.last().copied();
        let paren_at_start = parens.last().copied();
        let bracket_at_start = brackets.last().copied();
        let mut code = vec![b' '; line.len()];
        let mut line_comment_col = None;
        let mut index = 0usize;
        while index < line.len() {
            if comment.is_some() {
                if line.get(index..index.saturating_add(2)) == Some(b"*/") {
                    comment = None;
                    index += 2;
                } else {
                    index += 1;
                }
                continue;
            }
            if let Some(state) = raw.as_ref() {
                if line.get(index..index.saturating_add(state.close.len())) == Some(state.close.as_slice()) {
                    index += state.close.len();
                    raw = None;
                } else {
                    index += 1;
                }
                continue;
            }
            if line.get(index..index.saturating_add(2)) == Some(b"/*") {
                comment = Some(Pos { line: row, byte: index });
                index += 2;
                continue;
            }
            if line.get(index..index.saturating_add(2)) == Some(b"//") {
                line_comment_col = Some(index);
                break;
            }
            if line.get(index..index.saturating_add(2)) == Some(b"R\"") {
                let search_end = line.len().min(index.saturating_add(20));
                if let Some(relative) = line[index + 2..search_end].iter().position(|byte| *byte == b'(') {
                    let paren = index + 2 + relative;
                    let delimiter = &line[index + 2..paren];
                    let mut close = Vec::with_capacity(delimiter.len().saturating_add(2));
                    close.push(b')');
                    close.extend_from_slice(delimiter);
                    close.push(b'"');
                    raw = Some(RawState { start: Pos { line: row, byte: index }, close });
                    index = paren + 1;
                    continue;
                }
            }
            if matches!(line[index], b'"' | b'\'') {
                let quote = line[index];
                index += 1;
                while index < line.len() {
                    if line[index] == b'\\' && index + 1 < line.len() {
                        index += 2;
                    } else if line[index] == quote {
                        index += 1;
                        break;
                    } else {
                        index += 1;
                    }
                }
                continue;
            }
            let byte = line[index];
            code[index] = byte;
            let pos = Pos { line: row, byte: index };
            match byte {
                b'{' => braces.push(pos),
                b'}' => {
                    if let Some(open) = braces.pop() {
                        scan.brace_pairs.push((open, pos));
                    }
                }
                b'(' => parens.push(pos),
                b')' => {
                    if let Some(open) = parens.pop() {
                        scan.paren_pairs.push((open, pos));
                    }
                }
                b'[' => brackets.push(pos),
                b']' => {
                    if let Some(open) = brackets.pop() {
                        scan.bracket_pairs.push((open, pos));
                    }
                }
                _ => {}
            }
            index += 1;
        }
        scan.lines.push(CLine {
            code,
            comment_at_start,
            raw_at_start: raw_at_start.or_else(|| raw.as_ref().map(|state| state.start)),
            brace_at_start,
            brace_at_end: braces.last().copied(),
            paren_at_start,
            paren_at_end: parens.last().copied(),
            bracket_at_start,
            line_comment_col,
            raw_active: raw.is_some(),
        });
    }
    scan
}

fn find_start_brace(scan: &CScan, line: usize) -> Option<Pos> {
    if let Some(pos) = scan.lines.get(line).and_then(|info| info.brace_at_start) {
        return Some(pos);
    }
    for previous in (0..line).rev() {
        if let Some(pos) = scan.lines[previous].brace_at_end {
            return Some(pos);
        }
    }
    None
}

fn find_match_paren(scan: &CScan, line: usize, max_lines: usize) -> Option<Pos> {
    for previous in (0..=line).rev() {
        if let Some(pos) = scan.lines.get(previous).and_then(|info| info.paren_at_end) {
            if line.saturating_sub(pos.line) <= max_lines {
                return Some(pos);
            }
        }
    }
    None
}

fn find_start_comment(scan: &CScan, line: usize, max_lines: usize) -> Option<Pos> {
    let pos = scan.lines.get(line)?.comment_at_start?;
    (line.saturating_sub(pos.line) <= max_lines).then_some(pos)
}

fn find_start_rawstring(scan: &CScan, line: usize, max_lines: usize) -> Option<Pos> {
    let pos = scan.lines.get(line)?.raw_at_start?;
    (line.saturating_sub(pos.line) <= max_lines).then_some(pos)
}

fn first_nonblank(bytes: &[u8]) -> usize {
    bytes.iter().position(|byte| !matches!(byte, b' ' | b'\t')).unwrap_or(bytes.len())
}

fn trim_code(code: &[u8]) -> &[u8] {
    let start = code.iter().position(|byte| *byte != b' ').unwrap_or(code.len());
    let end = code.iter().rposition(|byte| *byte != b' ').map_or(start, |index| index + 1);
    &code[start..end]
}

fn has_code(info: &CLine) -> bool {
    !trim_code(&info.code).is_empty()
}

fn visual_col(line: &[u8], byte: usize, tabstop: usize) -> usize {
    let mut col = 0usize;
    for current in line.iter().copied().take(byte.min(line.len())) {
        col = if current == b'\t' {
            col.saturating_add(tab_padding(col, tabstop))
        } else {
            col.saturating_add(1)
        };
    }
    col
}

fn add_signed(base: usize, amount: i64) -> usize {
    if amount >= 0 {
        base.saturating_add(usize::try_from(amount).unwrap_or(usize::MAX))
    } else {
        base.saturating_sub(usize::try_from(amount.saturating_neg()).unwrap_or(usize::MAX))
    }
}

fn is_identifier(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn starts_word(line: &[u8], word: &[u8]) -> bool {
    line.starts_with(word) && line.get(word.len()).is_none_or(|byte| !is_identifier(*byte))
}

fn starts_if(line: &[u8]) -> bool {
    starts_word(line, b"if") || starts_word(line, b"while") || starts_word(line, b"for")
}

fn starts_else(line: &[u8]) -> bool {
    starts_word(line, b"else")
}

fn while_do_tail(line: &[u8]) -> bool {
    trim_code(line).ends_with(b"do")
}

fn is_case(line: &[u8]) -> bool {
    let line = trim_code(line);
    line.starts_with(b"case ") || starts_word(line, b"default")
}

fn is_control_head(code: &[u8]) -> bool {
    let line = trim_code(code);
    starts_if(line) || starts_word(line, b"switch") || while_do_tail(code)
}

fn is_cinword(source: &[u8], opts: &IndentOptions) -> bool {
    let line = trim_code(&source[first_nonblank(source)..]);
    opts.cinwords.split(',').filter(|word| !word.is_empty()).any(|word| starts_word(line, word.as_bytes()))
}

fn line_terminator(code: &[u8], is_else: bool, include_comma: bool) -> Option<u8> {
    let line = trim_code(code);
    if line.is_empty() {
        return None;
    }
    let mut rest_empty = true;
    for byte in line.iter().rev() {
        if *byte == b' ' {
            continue;
        }
        if !is_else && (*byte == b';' || *byte == b'}' || (include_comma && *byte == b',')) && rest_empty {
            return Some(*byte);
        }
        rest_empty = false;
        if *byte != b' ' {
            break;
        }
    }
    None
}

fn previous_scope_line(scan: &CScan, before: usize, scope: Option<Pos>) -> Option<usize> {
    let mut cursor = before;
    while cursor > 0 {
        cursor -= 1;
        let info = &scan.lines[cursor];
        if !has_code(info) && leading_len(&[]) == 0 {
            continue;
        }
        if let Some(scope) = scope {
            let direct = info.brace_at_start == Some(scope) || info.brace_at_end == Some(scope);
            if !direct {
                continue;
            }
        }
        return Some(cursor);
    }
    None
}

fn statement_start(scan: &CScan, line: usize, scope: Option<Pos>) -> usize {
    let mut cursor = line;
    loop {
        let code = &scan.lines[cursor].code;
        if is_control_head(code) || starts_else(trim_code(code)) {
            return cursor;
        }
        let Some(previous) = previous_scope_line(scan, cursor, scope) else {
            break;
        };
        if previous >= cursor {
            break;
        }
        // The statement that `cursor` continues can begin no earlier than
        // `cursor` when the line above terminates a statement (`cin_isterminated`
        // nonzero), or when the line above is the scope's own opening brace —
        // the first statement of the block starts at `cursor`.
        let previous_code = &scan.lines[previous].code;
        if line_terminator(previous_code, false, true).is_some()
            || scope.is_some_and(|scope| previous <= scope.line)
        {
            break;
        }
        cursor = previous;
    }
    cursor
}

fn find_matching_if(scan: &CScan, line: usize, scope: Option<Pos>) -> Option<usize> {
    let mut cursor = line;
    while let Some(previous) = previous_scope_line(scan, cursor, scope) {
        if starts_if(trim_code(&scan.lines[previous].code)) {
            return Some(previous);
        }
        cursor = previous;
    }
    None
}

fn find_matching_do(scan: &CScan, line: usize, scope: Option<Pos>) -> Option<usize> {
    let mut cursor = line;
    while let Some(previous) = previous_scope_line(scan, cursor, scope) {
        if while_do_tail(&scan.lines[previous].code) {
            return Some(previous);
        }
        cursor = previous;
    }
    None
}

fn matching_open_for_last_close(scan: &CScan, line: usize) -> Option<Pos> {
    let current = trim_code(&scan.lines[line].code);
    let close = *current.last()?;
    let open = match close {
        b'}' => b'{',
        b')' => b'(',
        b']' => b'[',
        _ => return None,
    };
    for (open_pos, close_pos) in scan
        .brace_pairs
        .iter()
        .chain(scan.paren_pairs.iter())
        .chain(scan.bracket_pairs.iter())
    {
        if close_pos.line == line && scan.lines[line].code.get(close_pos.byte) == Some(&close) {
            return Some(*open_pos);
        }
    }
    None
}

fn statement_start_for_block(scan: &CScan, open: Pos) -> usize {
    let info = &scan.lines[open.line];
    let first = trim_code(&info.code).first().copied();
    let parent = info.brace_at_start;
    if starts_else(&info.code) {
        return find_matching_if(scan, open.line, parent).unwrap_or(open.line);
    }
    if first == Some(b'{') {
        if let Some(previous) = previous_scope_line(scan, open.line, parent) {
            let code = &scan.lines[previous].code;
            if is_control_head(code) || line_terminator(code, false, true).is_none() {
                return statement_start(scan, previous, parent);
            }
        }
        return open.line;
    }
    statement_start(scan, open.line, parent)
}

fn logical_statement_start(scan: &CScan, line: usize, scope: Option<Pos>) -> usize {
    if let Some(open) = matching_open_for_last_close(scan, line) {
        return statement_start_for_block(scan, open);
    }
    if while_do_tail(&scan.lines[line].code) {
        return find_matching_do(scan, line, scope).unwrap_or(line);
    }
    statement_start(scan, line, scope)
}

fn unwind_unbraced_controls(scan: &CScan, mut line: usize, scope: Option<Pos>) -> usize {
    loop {
        let Some(previous) = previous_scope_line(scan, line, scope) else { break };
        let code = &scan.lines[previous].code;
        if !is_control_head(code) || code.contains(&b'{') || line_terminator(code, false, true).is_some() {
            break;
        }
        line = statement_start(scan, previous, scope);
    }
    line
}

fn nearest_case(scan: &CScan, before: usize, scope: Pos) -> Option<usize> {
    let mut cursor = before;
    while let Some(line) = previous_scope_line(scan, cursor, Some(scope)) {
        cursor = line;
        if is_case(&scan.lines[line].code) {
            return Some(line);
        }
    }
    None
}

#[derive(Clone, Debug, Default)]
struct CommentLeaders {
    start: Vec<u8>,
    start_offset: i64,
    middle: Vec<u8>,
    end: Vec<u8>,
    end_offset: i64,
}

fn comment_leaders(value: &str) -> CommentLeaders {
    let mut result = CommentLeaders::default();
    for part in value.split(',') {
        let Some((flags, text)) = part.split_once(':') else { continue };
        let offset = flags
            .trim_start_matches(|ch: char| ch.is_ascii_alphabetic())
            .parse::<i64>()
            .unwrap_or(0);
        if flags.starts_with('s') {
            result.start = text.as_bytes().to_vec();
            result.start_offset = offset;
        } else if flags.starts_with('m') {
            result.middle = text.as_bytes().to_vec();
        } else if flags.starts_with('e') {
            result.end = text.as_bytes().to_vec();
            result.end_offset = offset;
        }
    }
    result
}

fn comment_indent(lines: &[Vec<u8>], scan: &CScan, line: usize, start: Pos, opts: &IndentOptions) -> usize {
    let opener_col = visual_col(&lines[start.line], start.byte, opts.tabstop);
    let current = &lines[line][first_nonblank(&lines[line])..];
    let leaders = comment_leaders(&opts.comments);
    if !leaders.middle.is_empty() && current.starts_with(&leaders.middle) && !current.starts_with(&leaders.end) {
        let mut amount = opener_col;
        if line > 0 {
            let previous = &lines[line - 1][first_nonblank(&lines[line - 1])..];
            if previous.starts_with(&leaders.start) || previous.starts_with(&leaders.middle) {
                amount = indent_columns(&lines[line - 1], opts);
            }
        }
        return add_signed(amount, leaders.start_offset);
    }
    if !leaders.end.is_empty() && current.starts_with(&leaders.end) && !current.starts_with(&leaders.middle) {
        return add_signed(indent_columns(&lines[line - 1], opts), leaders.end_offset);
    }
    if current.first() == Some(&b'*') {
        if line > 0 {
            return indent_columns(&lines[line - 1], opts).max(add_signed(opener_col, 1));
        }
        return add_signed(opener_col, 1);
    }
    for previous in (start.line + 1..line).rev() {
        if !trim_code(&scan.lines[previous].code).is_empty() || leading_len(&lines[previous]) < lines[previous].len() {
            return indent_columns(&lines[previous], opts);
        }
    }
    let after = start.byte.saturating_add(2);
    let opener = &lines[start.line];
    if let Some(first_text) = opener.get(after..).and_then(|tail| {
        tail.iter().position(|byte| !matches!(byte, b' ' | b'\t')).map(|offset| after + offset)
    }) {
        visual_col(opener, first_text, opts.tabstop)
    } else {
        add_signed(opener_col, opts.cinoptions.in_comment)
    }
}

fn line_comment_indent(lines: &[Vec<u8>], scan: &CScan, line: usize, opts: &IndentOptions) -> Option<usize> {
    let current = &lines[line][first_nonblank(&lines[line])..];
    if !current.starts_with(b"//") {
        return None;
    }
    for previous in (0..line).rev() {
        if let Some(byte) = scan.lines[previous].line_comment_col {
            return Some(visual_col(&lines[previous], byte, opts.tabstop));
        }
        if has_code(&scan.lines[previous]) {
            break;
        }
    }
    None
}

fn paren_indent(lines: &[Vec<u8>], scan: &CScan, line: usize, open: Pos, opts: &IndentOptions) -> usize {
    let current = trim_code(&scan.lines[line].code);
    if current.contains(&b')') {
        return visual_col(&lines[open.line], open.byte, opts.tabstop);
    }
    for previous in (open.line + 1..line).rev() {
        let info = &scan.lines[previous];
        if info.paren_at_start == Some(open) && has_code(info) {
            return indent_columns(&lines[previous], opts);
        }
    }
    visual_col(&lines[open.line], open.byte, opts.tabstop)
}

fn brace_base(lines: &[Vec<u8>], scan: &CScan, open: Pos, opts: &IndentOptions) -> usize {
    let info = &scan.lines[open.line];
    if first_nonblank(&info.code) == open.byte {
        visual_col(&lines[open.line], open.byte, opts.tabstop)
    } else {
        indent_columns(&lines[statement_start_for_block(scan, open)], opts)
    }
}

fn cindent_cols(lines: &[Vec<u8>], lnum: usize, opts: &IndentOptions) -> usize {
    match cindent(lines, lnum, opts) {
        IndentAmount::Columns(value) => value,
        IndentAmount::LeaveAlone => lines
            .get(lnum.saturating_sub(1))
            .map_or(0, |line| indent_columns(line, opts)),
    }
}

fn brace_indent(lines: &[Vec<u8>], scan: &CScan, line: usize, open: Pos, opts: &IndentOptions) -> usize {
    let current = trim_code(&scan.lines[line].code);
    let base = brace_base(lines, scan, open, opts);
    if current.first() == Some(&b'}') {
        return base;
    }
    if starts_else(current) {
        if let Some(matched) = find_matching_if(scan, line, Some(open)) {
            return indent_columns(&lines[matched], opts);
        }
    }
    if while_do_tail(current) {
        if let Some(matched) = find_matching_do(scan, line, Some(open)) {
            return indent_columns(&lines[matched], opts);
        }
    }
    if is_case(current) {
        if let Some(previous) = nearest_case(scan, line, open) {
            return cindent_cols(lines, previous + 1, opts);
        }
        return add_signed(base, opts.cinoptions.case_label);
    }
    let level = opts.cinoptions.level;
    let scope_amount = add_signed(base, level);
    let previous_case = nearest_case(scan, line, open);
    let Some(previous) = previous_scope_line(scan, line, Some(open)) else {
        return scope_amount;
    };
    let previous_code = &scan.lines[previous].code;
    if is_case(previous_code) {
        return add_signed(cindent_cols(lines, previous + 1, opts), opts.cinoptions.case_code);
    }
    if current.first() == Some(&b'{') {
        let start = statement_start(scan, previous, Some(open));
        if is_control_head(&scan.lines[start].code) || line_terminator(previous_code, false, true).is_none() {
            return indent_columns(&lines[start], opts);
        }
        return scope_amount;
    }
    if line_terminator(previous_code, false, true).is_none() {
        let start = statement_start(scan, previous, Some(open));
        if is_control_head(&scan.lines[start].code) {
            return add_signed(cindent_cols(lines, start + 1, opts), level);
        }
        return add_signed(cindent_cols(lines, start + 1, opts), opts.cinoptions.continuation);
    }
    if line_terminator(previous_code, false, true) == Some(b';') {
        let anchor = logical_statement_start(scan, previous, Some(open));
        if anchor <= open.line {
            return scope_amount;
        }
    }
    let logical = logical_statement_start(scan, previous, Some(open));
    let anchor = unwind_unbraced_controls(scan, logical, Some(open));
    if previous_case.is_some_and(|case| anchor <= case) {
        let case = previous_case.expect("checked");
        return add_signed(indent_columns(&lines[case], opts), opts.cinoptions.case_code);
    }
    cindent_cols(lines, anchor + 1, opts)
}

fn top_level_indent(lines: &[Vec<u8>], scan: &CScan, line: usize, opts: &IndentOptions) -> usize {
    let current = trim_code(&scan.lines[line].code);
    if current.first() == Some(&b'{') {
        return 0;
    }
    let Some(previous) = previous_scope_line(scan, line, None) else { return 0 };
    let previous_code = &scan.lines[previous].code;
    if line_terminator(previous_code, false, true).is_none() {
        let start = statement_start(scan, previous, None);
        if is_control_head(&scan.lines[start].code) {
            return add_signed(cindent_cols(lines, start + 1, opts), opts.cinoptions.level);
        }
        return add_signed(cindent_cols(lines, start + 1, opts), opts.cinoptions.continuation);
    }
    let logical = logical_statement_start(scan, previous, None);
    cindent_cols(lines, unwind_unbraced_controls(scan, logical, None) + 1, opts)
}

#[must_use]
pub(crate) fn cindent(lines: &[Vec<u8>], lnum: usize, opts: &IndentOptions) -> IndentAmount {
    if lnum == 0 || lnum > lines.len() {
        return IndentAmount::LeaveAlone;
    }
    if lnum == 1 {
        return IndentAmount::Columns(0);
    }
    let line = lnum - 1;
    let scan = scan_c(lines, line);
    if scan.lines.get(line).is_some_and(|info| info.raw_active) {
        return IndentAmount::LeaveAlone;
    }
    let current = &lines[line][first_nonblank(&lines[line])..];
    if current.first() == Some(&b'#') {
        return IndentAmount::Columns(0);
    }
    if let Some(start) = find_start_comment(&scan, line, opts.cinoptions.maxcomment) {
        let mut amount = comment_indent(lines, &scan, line, start, opts);
        if let Some(brace) = find_start_brace(&scan, line) {
            amount = amount.max(brace_indent(lines, &scan, line, brace, opts));
        }
        return IndentAmount::Columns(amount);
    }
    if let Some(amount) = line_comment_indent(lines, &scan, line, opts) {
        return IndentAmount::Columns(amount);
    }
    if trim_code(&scan.lines[line].code).first() == Some(&b']') {
        if let Some(open) = scan.lines[line].bracket_at_start {
            return IndentAmount::Columns(indent_columns(&lines[open.line], opts));
        }
    }
    let paren = find_match_paren(&scan, line, opts.cinoptions.maxparen);
    let brace = find_start_brace(&scan, line);
    let amount = match (paren, brace) {
        (Some(paren), Some(brace)) => match paren.cmp(&brace) {
            Ordering::Less => paren_indent(lines, &scan, line, paren, opts),
            Ordering::Greater => brace_indent(lines, &scan, line, brace, opts),
            Ordering::Equal => brace_indent(lines, &scan, line, brace, opts),
        },
        (Some(paren), None) => paren_indent(lines, &scan, line, paren, opts),
        (None, Some(brace)) => brace_indent(lines, &scan, line, brace, opts),
        (None, None) => top_level_indent(lines, &scan, line, opts),
    };
    IndentAmount::Columns(amount)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LispOpen {
    pos: Pos,
    kind: u8,
}

#[derive(Clone, Debug)]
struct LispLine {
    code: Vec<u8>,
    open_at_start: Option<LispOpen>,
    open_at_end: Option<LispOpen>,
}

fn scan_lisp(lines: &[Vec<u8>], through: usize) -> Vec<LispLine> {
    let mut result = Vec::with_capacity(through.saturating_add(1));
    let mut stack = Vec::<LispOpen>::new();
    for (row, line) in lines.iter().enumerate().take(through.saturating_add(1)) {
        let open_at_start = stack.last().copied();
        let mut code = vec![b' '; line.len()];
        let mut index = 0usize;
        while index < line.len() {
            match line[index] {
                b';' => break,
                b'\\' if index + 1 < line.len() => index += 2,
                b'"' => {
                    index += 1;
                    while index < line.len() {
                        if line[index] == b'\\' && index + 1 < line.len() {
                            index += 2;
                        } else if line[index] == b'"' {
                            index += 1;
                            break;
                        } else {
                            index += 1;
                        }
                    }
                }
                b'(' | b'[' => {
                    code[index] = line[index];
                    stack.push(LispOpen { pos: Pos { line: row, byte: index }, kind: line[index] });
                    index += 1;
                }
                b')' | b']' => {
                    code[index] = line[index];
                    let expected = if line[index] == b')' { b'(' } else { b'[' };
                    if stack.last().is_some_and(|open| open.kind == expected) {
                        stack.pop();
                    }
                    index += 1;
                }
                byte => {
                    code[index] = byte;
                    index += 1;
                }
            }
        }
        result.push(LispLine { code, open_at_start, open_at_end: stack.last().copied() });
    }
    result
}

fn lispword_matches(line: &[u8], start: usize, opts: &IndentOptions) -> bool {
    opts.lispwords.split(',').filter(|word| !word.is_empty()).any(|word| {
        let bytes = word.as_bytes();
        line.get(start..start.saturating_add(bytes.len())) == Some(bytes)
            && line.get(start.saturating_add(bytes.len())).is_none_or(|byte| matches!(byte, b' ' | b'\t'))
    })
}

#[must_use]
pub(crate) fn lisp_indent(lines: &[Vec<u8>], lnum: usize, opts: &IndentOptions) -> IndentAmount {
    if lnum == 0 || lnum > lines.len() {
        return IndentAmount::LeaveAlone;
    }
    let line = lnum - 1;
    let scan = scan_lisp(lines, line);
    let Some(open) = scan[line].open_at_start else {
        return IndentAmount::Columns(0);
    };
    for previous in (0..line).rev() {
        if trim_code(&scan[previous].code).is_empty() {
            continue;
        }
        if scan[previous].open_at_end == scan[line].open_at_start && scan[line].open_at_start.is_some() {
            let current_trim = trim_code(&scan[line].code);
            if !matches!(current_trim.first(), Some(b'(' | b'[')) {
                return IndentAmount::Columns(indent_columns(&lines[previous], opts).saturating_add(1));
            }
            break;
        }
    }
    let source = &lines[open.pos.line];
    let mut amount = visual_col(source, open.pos.byte, opts.tabstop);
    if lispword_matches(source, open.pos.byte.saturating_add(1), opts) {
        return IndentAmount::Columns(amount.saturating_add(2));
    }
    let mut cursor = open.pos.byte.saturating_add(1);
    amount = amount.saturating_add(1);
    let mut firsttry = amount;
    while cursor < source.len() && matches!(source[cursor], b' ' | b'\t') {
        amount = amount.saturating_add(if source[cursor] == b'\t' { tab_padding(amount, opts.tabstop) } else { 1 });
        cursor += 1;
    }
    if cursor < source.len() && source[cursor] != b';' {
        if !matches!(source[cursor], b'(' | b'[') {
            firsttry = firsttry.saturating_add(1);
        }
        if !matches!(source[cursor], b'"' | b'\'' | b'#' | b'0'..=b'9') {
            let mut quotes = false;
            let mut parens = 0i64;
            while cursor < source.len() && (!matches!(source[cursor], b' ' | b'\t') || quotes || parens != 0) {
                match source[cursor] {
                    b'"' => quotes = !quotes,
                    b'(' | b'[' if !quotes => parens += 1,
                    b')' | b']' if !quotes => parens -= 1,
                    b'\\' if cursor + 1 < source.len() => {
                        amount = amount.saturating_add(1);
                        cursor += 1;
                    }
                    _ => {}
                }
                amount = amount.saturating_add(1);
                cursor += 1;
            }
        }
        while cursor < source.len() && matches!(source[cursor], b' ' | b'\t') {
            amount = amount.saturating_add(if source[cursor] == b'\t' { tab_padding(amount, opts.tabstop) } else { 1 });
            cursor += 1;
        }
        if cursor == source.len() || source[cursor] == b';' {
            amount = firsttry;
        }
    }
    IndentAmount::Columns(amount)
}

#[must_use]
pub(crate) fn smart_newline_indent(source_prefix: &[u8], smart_trigger: bool, opts: &IndentOptions) -> Vec<u8> {
    if opts.paste {
        return Vec::new();
    }
    let do_smart = opts.smartindent && !opts.cindent && opts.indentexpr.is_empty();
    if !opts.autoindent && !opts.copyindent && !do_smart {
        return Vec::new();
    }
    let mut target = indent_columns(source_prefix, opts);
    if do_smart && smart_trigger {
        target = target.saturating_add(opts.shiftwidth);
    }
    newline_whitespace_for(target, source_prefix, opts)
}

#[must_use]
pub(crate) fn smart_source_trigger(source: &[u8], backward: bool, opts: &IndentOptions) -> bool {
    let trimmed_start = &source[first_nonblank(source)..];
    if backward {
        return trimmed_start.first() == Some(&b'}');
    }
    let trimmed_end = source
        .iter()
        .rposition(|byte| !matches!(byte, b' ' | b'\t'))
        .map_or(&[][..], |end| &source[..=end]);
    trimmed_end.last() == Some(&b'{') || is_cinword(source, opts)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CinTrigger {
    OpenForward,
    OpenBackward,
    Else,
    OpenBrace,
    CloseBrace,
    Hash,
    CloseBracket,
    CloseParen,
    Colon,
}

#[must_use]
pub(crate) fn cinkeys_trigger(opts: &IndentOptions, trigger: CinTrigger) -> bool {
    let keys = if opts.indentexpr.is_empty() { &opts.cinkeys } else { &opts.indentkeys };
    let wanted = match trigger {
        CinTrigger::OpenForward => "o",
        CinTrigger::OpenBackward => "O",
        CinTrigger::Else => "e",
        CinTrigger::OpenBrace => "0{",
        CinTrigger::CloseBrace => "0}",
        CinTrigger::Hash => "0#",
        CinTrigger::CloseBracket => "0]",
        CinTrigger::CloseParen => "0)",
        CinTrigger::Colon => ":",
    };
    keys.split(',').any(|entry| entry.trim() == wanted)
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum IndentExprError {
    #[error("indent expression failed: {0}")]
    Failed(String),
    /// No expression evaluator is wired (e.g. the typeahead drain is using
    /// [`NullExprEval`]). Degrade to "expression ignored, indent unchanged"
    /// instead of aborting the open/Enter/reindent operation.
    #[error("indent expression evaluation is unavailable")]
    Unavailable,
}

/// Read-only indent evaluation view over the live editor and a staged line overlay.
///
/// During a reindent plan, expression and builtin indenters observe `lines`
/// (including earlier planned indentation) without being able to mutate the
/// target buffer through this context.
pub struct IndentEvalContext<'a> {
    editor: &'a Editor,
    buffer: BufHandle,
    lines: &'a [Vec<u8>],
}

impl<'a> IndentEvalContext<'a> {
    /// Borrows the editor, target buffer handle, and staged lines for one evaluation.
    #[must_use]
    pub const fn new(editor: &'a Editor, buffer: BufHandle, lines: &'a [Vec<u8>]) -> Self {
        Self { editor, buffer, lines }
    }

    /// Editor providing options and buffer metadata (not buffer text mutation).
    #[must_use]
    pub const fn editor(&self) -> &'a Editor {
        self.editor
    }

    /// Buffer whose indentation is being planned.
    #[must_use]
    pub const fn buffer(&self) -> BufHandle {
        self.buffer
    }

    /// Staged line overlay observed by indent calculations.
    #[must_use]
    pub const fn lines(&self) -> &'a [Vec<u8>] {
        self.lines
    }
}

pub trait ExprEval {
    fn eval_indentexpr(
        &mut self,
        context: &IndentEvalContext<'_>,
        lnum: usize,
        expression: &str,
    ) -> Result<i64, IndentExprError>;
}

pub struct NullExprEval;

impl ExprEval for NullExprEval {
    fn eval_indentexpr(
        &mut self,
        _context: &IndentEvalContext<'_>,
        _lnum: usize,
        _expression: &str,
    ) -> Result<i64, IndentExprError> {
        Err(IndentExprError::Failed(
            "indent expression evaluation is unavailable".to_owned(),
        ))
    }
}

/// Adapter that converts any indent-expression failure from the wrapped
/// evaluator into [`IndentExprError::Unavailable`], so call sites degrade it
/// to "expression ignored, indent unchanged" instead of aborting with E523.
pub struct IgnoreExprEval<'a> {
    inner: &'a mut dyn ExprEval,
}

impl<'a> IgnoreExprEval<'a> {
    /// Wraps `inner` so its indent-expression errors are treated as ignored.
    #[must_use]
    pub fn new(inner: &'a mut dyn ExprEval) -> Self {
        Self { inner }
    }
}

impl<'a> ExprEval for IgnoreExprEval<'a> {
    fn eval_indentexpr(
        &mut self,
        context: &IndentEvalContext<'_>,
        lnum: usize,
        expression: &str,
    ) -> Result<i64, IndentExprError> {
        self.inner
            .eval_indentexpr(context, lnum, expression)
            .map_err(|_| IndentExprError::Unavailable)
    }
}


#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Method {
    Cindent,
    Lisp,
    Expr,
}

#[must_use]
pub(crate) fn resolve_options_method(opts: &IndentOptions) -> Method {
    if !opts.indentexpr.is_empty() {
        Method::Expr
    } else if opts.lisp {
        Method::Lisp
    } else if opts.cindent {
        Method::Cindent
    } else {
        Method::Cindent
    }
}

#[must_use]
pub(crate) fn equalprg(editor: &Editor, buffer: BufHandle) -> Option<String> {
    match editor.options().get_buffer(buffer, "equalprg") {
        Ok(OptionValue::String(value)) if !value.is_empty() => Some(value.clone()),
        _ => None,
    }
}

#[must_use]
pub(crate) fn resolve_method(editor: &Editor, buffer: BufHandle) -> Option<Method> {
    if equalprg(editor, buffer).is_some() {
        None
    } else {
        Some(resolve_options_method(&IndentOptions::capture(editor, buffer)))
    }
}

pub(crate) fn amount_for(
    context: &IndentEvalContext<'_>,
    lnum: usize,
    method: Method,
    opts: &IndentOptions,
    eval: &mut dyn ExprEval,
) -> Result<IndentAmount, IndentExprError> {
    let amount = match method {
        Method::Cindent => cindent(context.lines(), lnum, opts),
        Method::Lisp => lisp_indent(context.lines(), lnum, opts),
        Method::Expr => match eval.eval_indentexpr(context, lnum, &opts.indentexpr) {
            Ok(value) if value >= 0 => {
                IndentAmount::Columns(usize::try_from(value).unwrap_or(usize::MAX))
            }
            Ok(_) => context.lines().get(lnum.saturating_sub(1)).map_or(
                IndentAmount::LeaveAlone,
                |line| IndentAmount::Columns(indent_columns(line, opts)),
            ),
            // No real evaluator is wired; ignore the expression and keep the
            // existing indent instead of aborting o/O/Enter/==.
            Err(IndentExprError::Unavailable) => IndentAmount::LeaveAlone,
            Err(error) => return Err(error),
        },
    };
    Ok(amount)
}

pub(crate) fn fix_line_indent(
    context: &IndentEvalContext<'_>,
    lnum: usize,
    trigger: CinTrigger,
    opts: &IndentOptions,
    eval: &mut dyn ExprEval,
) -> Result<Option<Vec<u8>>, IndentExprError> {
    // cinkeys only fire when an indent method is actually enabled; default
    // cinkeys include `o`/`O` and must not invent cindent for plain buffers.
    if opts.indentexpr.is_empty() && !opts.lisp && !opts.cindent {
        return Ok(None);
    }
    if !cinkeys_trigger(opts, trigger) {
        return Ok(None);
    }
    let method = resolve_options_method(opts);
    let amount = match amount_for(context, lnum, method, opts, eval)? {
        IndentAmount::Columns(value) => value,
        IndentAmount::LeaveAlone => return Ok(None),
    };
    Ok(Some(whitespace_for(amount, context.lines().get(lnum.saturating_sub(1)).map_or(&[], Vec::as_slice), opts)))
}


