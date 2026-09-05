//! The buffer-search builtins: `search()`, `searchpair()`,
//! `searchpairpos()`, and `searchcount()` (`eval/funcs.c` `f_search`,
//! `searchpair_cmn`, `do_searchpair`, and `search.c` `f_searchcount`).

use crate::excmd_exec::ExEditorAccess;
use std::time::{Duration, Instant};

use ox_eval::{EvalError, Evaluator, Parser as ExprParser, Scope};
use ox_text::Position;
use ox_types::{OxStr, Typval, WinHandle};

use crate::Editor;
use crate::excmd_exec::{EvalHost, VimRegex, buffer_lines};
use crate::options::OptionValue;
use crate::script::FileIO;
use crate::search::{
    CandidateScan, PairBranch, PairProgram, SearchCountState, SearchDirection, SearchError,
    SearchState, SearchText, Step, editor_position,
};
use ox_regex::Magic;

use super::input_string_arg;
use super::position::number_value;

/// Routes the Search family.
pub(crate) fn call<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    name: &str,
    args: &[Typval],
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    check_arity(name, args.len())?;
    match name {
        "search" => host
            .access
            .with_ex_editor(|editor| search_from_cursor(editor, args)),
        "searchpair" => searchpair_family(host, args, scope, false),
        "searchpairpos" => searchpair_family(host, args, scope, true),
        "searchcount" => searchcount(host, args),
        _ => Err(EvalError::new(
            "E117",
            0,
            format!("Unknown function: {name}"),
        )),
    }
}

/// Enforces the generated `eval.lua` argument counts before a body runs.
fn check_arity(name: &str, count: usize) -> ox_eval::Result<()> {
    let spec = ox_eval::builtin_spec(name)
        .ok_or_else(|| EvalError::new("E117", 0, format!("Unknown function: {name}")))?;
    if count < spec.min_args {
        return Err(EvalError::new(
            "E119",
            0,
            format!("Not enough arguments for function: {name}"),
        ));
    }
    if spec.max_args.is_some_and(|maximum| count > maximum) {
        return Err(EvalError::new(
            "E118",
            0,
            format!("Too many arguments for function: {name}"),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// search()
// ---------------------------------------------------------------------------

/// `search()`: buffer search from the cursor. Behavior is unchanged from the
/// pre-family implementation; flags beyond `b`/`n`/`s`/`w`/`W` keep their
/// historical no-op status here.
fn search_from_cursor(editor: &mut Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    let pattern = input_string_arg(&args[0])?;
    let flags = args
        .get(1)
        .map(input_string_arg)
        .transpose()?
        .unwrap_or_else(|| OxStr::from(""));
    let flags = flags.to_string_lossy();
    if flags.contains('n') && flags.contains('s') {
        return Ok(Typval::Number(0));
    }
    let direction = if flags.contains('b') {
        SearchDirection::Backward
    } else {
        SearchDirection::Forward
    };
    let wrapscan = if flags.contains('W') {
        false
    } else if flags.contains('w') {
        true
    } else {
        option_bool(editor, "wrapscan", true)
    };
    let Some(window) = editor.current_window() else {
        return Ok(Typval::Number(0));
    };
    let (buffer, cursor) = {
        let state = editor
            .window(window)
            .map_err(|error| EvalError::new("E117", 0, error.to_string()))?;
        (state.buffer, state.cursor)
    };
    let lines =
        buffer_lines(editor, buffer).map_err(|error| EvalError::new("E117", 0, error.clone()))?;
    let mut state = SearchState::default();
    let result = match state.search(
        &lines,
        cursor,
        &pattern.to_string_lossy(),
        direction,
        1,
        wrapscan,
    ) {
        Ok(result) => result,
        Err(SearchError::PatternNotFound(_)) => return Ok(Typval::Number(0)),
        Err(error) => return Err(search_error(&error)),
    };
    if !flags.contains('n') {
        editor
            .set_window_cursor(window, result.target)
            .map_err(|error| EvalError::new("E117", 0, error.to_string()))?;
    }
    Ok(Typval::Number(
        i64::try_from(result.target.lnum).unwrap_or(0),
    ))
}

// ---------------------------------------------------------------------------
// searchpair() / searchpairpos()
// ---------------------------------------------------------------------------

/// Parsed `searchpair()` flag set. Typed accessors replace upstream's bitmask
/// (`get_search_arg` plus `searchpair_cmn`'s rejections).
#[derive(Default)]
struct PairFlags {
    bits: u8,
    /// Last `w`/`W` wins; `None` defers to `'wrapscan'`.
    wrap_override: Option<bool>,
}

#[derive(Clone, Copy)]
enum PairFlag {
    Backward = 1 << 0,
    AtCursor = 1 << 1,
    NoMove = 1 << 2,
    SetPcMark = 1 << 3,
    Repeat = 1 << 4,
    ReturnCount = 1 << 5,
}

impl PairFlags {
    fn insert(&mut self, flag: PairFlag) {
        self.bits |= flag as u8;
    }

    fn contains(&self, flag: PairFlag) -> bool {
        self.bits & flag as u8 != 0
    }
}

/// Parses `searchpair()` flags. Unknown characters, `e`, `p`, and the `n`
/// plus `s` combination raise `E475` with the upstream argument strings.
fn parse_pair_flags(arg: Option<&Typval>) -> ox_eval::Result<PairFlags> {
    let Some(value) = arg else {
        return Ok(PairFlags::default());
    };
    let raw = input_string_arg(value)?.to_string_lossy().into_owned();
    let mut flags = PairFlags::default();
    let mut rejected = false;
    for (offset, character) in raw.char_indices() {
        match character {
            'b' => flags.insert(PairFlag::Backward),
            'w' => flags.wrap_override = Some(true),
            'W' => flags.wrap_override = Some(false),
            'c' => flags.insert(PairFlag::AtCursor),
            'm' => flags.insert(PairFlag::ReturnCount),
            'n' => flags.insert(PairFlag::NoMove),
            'r' => flags.insert(PairFlag::Repeat),
            's' => flags.insert(PairFlag::SetPcMark),
            // `z` parses generically but `do_searchpair()` applies no
            // column-only notion; `e` and `p` are rejected outright.
            'z' => {}
            'e' | 'p' => rejected = true,
            _ => return Err(e475(&raw[offset..])),
        }
    }
    if rejected || (flags.contains(PairFlag::NoMove) && flags.contains(PairFlag::SetPcMark)) {
        return Err(e475(&raw));
    }
    Ok(flags)
}

/// A parsed `skip` argument (`eval_expr_valid_arg` plus the empty-string
/// rule): omitted, number zero, and the empty string accept every candidate.
#[derive(Debug)]
enum Skip {
    /// Every candidate is accepted.
    None,
    /// A non-empty expression evaluated per candidate.
    Expression(String),
    /// A Funcref or Partial invoked with no arguments.
    Callable(Typval),
}

fn parse_skip(arg: Option<&Typval>) -> ox_eval::Result<Skip> {
    let Some(value) = arg else {
        return Ok(Skip::None);
    };
    match value {
        Typval::Number(0) => Ok(Skip::None),
        Typval::Funcref(_) | Typval::Partial(_) => Ok(Skip::Callable(value.clone())),
        other => {
            let source = input_string_arg(other)?.to_string_lossy().into_owned();
            if source.is_empty() {
                Ok(Skip::None)
            } else {
                Ok(Skip::Expression(source))
            }
        }
    }
}

/// Evaluates `skip` for the candidate the temporary cursor sits on; `true`
/// means the candidate is skipped. Funcref and Partial values go through
/// [`Evaluator::invoke`] so closure-registry identity and bound arguments
/// survive.
fn eval_skip<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    scope: &mut Scope,
    skip: &Skip,
) -> ox_eval::Result<bool> {
    match skip {
        Skip::None => Ok(false),
        Skip::Expression(source) => {
            let expression = ExprParser::new(source.as_bytes()).parse()?;
            let regex = VimRegex;
            let result = Evaluator::new(host, &regex).eval(&expression, scope)?;
            Ok(number_value(&result)? != 0)
        }
        Skip::Callable(callee) => {
            let regex = VimRegex;
            let result = Evaluator::new(host, &regex).invoke(callee.clone(), Vec::new(), scope)?;
            Ok(number_value(&result)? != 0)
        }
    }
}

/// `searchpair()` and `searchpairpos()`: one shared pair scan; `want_position`
/// projects the completed match to `[lnum, col]` instead of the line number.
fn searchpair_family<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
    scope: &mut Scope,
    want_position: bool,
) -> ox_eval::Result<Typval> {
    let start = pattern_arg(&args[0])?;
    let middle = pattern_arg(&args[1])?;
    let end = pattern_arg(&args[2])?;
    let flags = parse_pair_flags(args.get(3))?;
    let skip = parse_skip(args.get(4))?;
    let stop_line = parse_pair_bound(args.get(5))?;
    let timeout = parse_pair_bound(args.get(6))?;

    let Some(mut context) = host.access.with_ex_editor(search_context)? else {
        return Ok(if want_position {
            Typval::list(vec![Typval::Number(0), Typval::Number(0)])
        } else {
            Typval::Number(0)
        });
    };
    // `r` implies `W`; otherwise the last `w`/`W` wins over `'wrapscan'`.
    let request = PairRequest {
        direction: if flags.contains(PairFlag::Backward) {
            SearchDirection::Backward
        } else {
            SearchDirection::Forward
        },
        flags: &flags,
        skip: &skip,
        stop_line: stop_line.map(|line| usize::try_from(line).unwrap_or(usize::MAX)),
        deadline: count_deadline(timeout),
        wrap: !flags.contains(PairFlag::Repeat)
            && flags.wrap_override.unwrap_or_else(|| {
                host.access
                    .with_ex_editor(|editor| option_bool(editor, "wrapscan", true))
            }),
    };
    let program = PairProgram::compile(
        &start,
        &middle,
        &end,
        host.access
            .with_ex_editor(|editor| option_bool(editor, "ignorecase", false)),
    )
    .map_err(|error| search_error(&error))?;
    let outcome = run_pair(host, scope, &mut context, &program, &request)?;

    // A completed match leaves the cursor on it; failure or `n` restores the
    // entry position (`do_searchpair`'s tail).
    let completed = outcome.completed;
    let cursor = if flags.contains(PairFlag::NoMove) {
        context.cursor
    } else {
        completed.unwrap_or(context.cursor)
    };
    host.access
        .with_ex_editor(|editor| place_cursor(editor, context.window, cursor))?;
    if want_position {
        return Ok(match completed {
            Some(at) => Typval::list(vec![
                Typval::Number(i64::try_from(at.lnum).unwrap_or(i64::MAX)),
                Typval::Number(i64::try_from(at.col).map_or(i64::MAX, |col| col.saturating_add(1))),
            ]),
            None => Typval::list(vec![Typval::Number(0), Typval::Number(0)]),
        });
    }
    if flags.contains(PairFlag::ReturnCount) {
        return Ok(Typval::Number(outcome.count));
    }
    Ok(Typval::Number(match completed {
        Some(at) => i64::try_from(at.lnum).unwrap_or(i64::MAX),
        None => 0,
    }))
}

/// Everything one pair scan needs, resolved before the loop starts.
struct PairRequest<'a> {
    direction: SearchDirection,
    flags: &'a PairFlags,
    skip: &'a Skip,
    /// Inclusive one-based `stopline`.
    stop_line: Option<usize>,
    deadline: Option<Instant>,
    wrap: bool,
}

impl PairRequest<'_> {
    fn scanner<'a>(&self, text: &'a SearchText) -> CandidateScan<'a> {
        make_scanner(
            text,
            self.direction,
            self.stop_line,
            self.deadline,
            self.wrap,
        )
    }
}

/// Mutable state of one `do_searchpair` loop.
struct PairOutcome {
    /// Completed outer matches under `m`.
    count: i64,
    /// The last completed match position, if any.
    completed: Option<Position>,
}

struct SkipEvaluation {
    skip_candidate: bool,
    buffer_changed: bool,
}

/// Evaluates `skip` with the cursor on the candidate, then restores the logical
/// pair-loop cursor. The expression may mutate the buffer, which invalidates
/// both the search text and scanner held by `run_pair`.
fn evaluate_pair_skip<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    scope: &mut Scope,
    context: &SearchContext,
    skip: &Skip,
    candidate: Position,
    logical_cursor: Position,
) -> ox_eval::Result<SkipEvaluation> {
    host.access
        .with_ex_editor(|editor| place_cursor(editor, context.window, candidate))?;
    let tick = host
        .access
        .with_ex_editor(|editor| changedtick_of(editor, context.buffer));
    let skip_candidate = match eval_skip(host, scope, skip) {
        Ok(skip_candidate) => skip_candidate,
        Err(error) => {
            let _ = host
                .access
                .with_ex_editor(|editor| editor.set_window_cursor(context.window, context.cursor));
            return Err(error);
        }
    };
    host.access
        .with_ex_editor(|editor| place_cursor(editor, context.window, logical_cursor))?;
    Ok(SkipEvaluation {
        skip_candidate,
        buffer_changed: host
            .access
            .with_ex_editor(|editor| changedtick_of(editor, context.buffer))
            != tick,
    })
}

/// The nested start/middle/end scan. Each step finds one candidate, evaluates
/// `skip` on it, and applies the branch's nesting transition until the depth
/// returns to zero or the scan runs out.
fn run_pair<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    scope: &mut Scope,
    context: &mut SearchContext,
    program: &PairProgram,
    request: &PairRequest<'_>,
) -> ox_eval::Result<PairOutcome> {
    let entry = context.cursor;
    let mut text = SearchText::new(&context.lines).map_err(|error| search_error(&error))?;
    let mut scanner = Some(request.scanner(&text));
    let mut nest = 1usize;
    let mut nested = false;
    let mut first: Option<(usize, usize)> = None;
    let mut found: Option<(usize, usize)> = None;
    let mut completed: Option<Position> = None;
    let mut count = 0i64;
    let mut scan_from = text.byte_of(entry);
    let mut include = request.flags.contains(PairFlag::AtCursor);

    loop {
        let Some(active_scanner) = scanner.as_mut() else {
            unreachable!("the pair scanner is rebuilt before the next step");
        };
        let step = active_scanner
            .step_pair(program, nested, scan_from, include)
            .map_err(|error| search_error(&error))?;
        // The `c` inclusion applies to the first search only.
        include = false;
        let candidate = match step {
            Step::Found(candidate) => candidate,
            Step::Exhausted | Step::TimedOut => break,
        };
        // Finding the first candidate again means nothing was accepted: FAIL.
        let start_at = editor_position(candidate.span.start);
        let start_key = (start_at.lnum, start_at.col);
        if first.is_some_and(|first| first == start_key) {
            break;
        }
        if first.is_none() {
            first = Some(start_key);
        }
        // A repeated position (\zs backwards, zero-width) advances one
        // character before participating (`decl`/`incl`).
        let mut at = start_at;
        if found == Some(start_key) {
            let byte = match request.direction {
                SearchDirection::Forward => text.scalar_after(text.byte_of(at)),
                SearchDirection::Backward => text.scalar_before(text.byte_of(at)),
            };
            at = editor_position(crate::search::position_of(&text, byte));
        }
        found = Some((at.lnum, at.col));

        // Skip evaluation runs with the cursor on the candidate; both verdict
        // and error paths restore the logical loop cursor first, and an error
        // additionally puts the entry cursor back before propagating.
        if !matches!(request.skip, Skip::None) {
            let logical_cursor = completed.unwrap_or(entry);
            let evaluation =
                evaluate_pair_skip(host, scope, context, request.skip, at, logical_cursor)?;
            // A skip body may mutate the buffer: reload lines and rebuild
            // the search text and scanner before continuing.
            if evaluation.buffer_changed {
                let _ = scanner.take();
                context.lines = host
                    .access
                    .with_ex_editor(|editor| buffer_lines(editor, context.buffer))
                    .map_err(|error| EvalError::new("E117", 0, error.clone()))?;
                text = SearchText::new(&context.lines).map_err(|error| search_error(&error))?;
                scanner = Some(request.scanner(&text));
            }
            if evaluation.skip_candidate {
                scan_from = text.byte_of(at);
                continue;
            }
        }
        scan_from = text.byte_of(at);

        // Forward `start` and backward `end` open a nested level, which drops
        // `middle` from the candidate program; everything else closes one.
        let opens = match request.direction {
            SearchDirection::Forward => candidate.branch == PairBranch::Start,
            SearchDirection::Backward => candidate.branch == PairBranch::End,
        };
        if opens {
            nest += 1;
            nested = true;
        } else {
            nest -= 1;
            if nest == 1 {
                nested = false;
            }
        }

        if nest == 0 {
            if request.flags.contains(PairFlag::ReturnCount) {
                count = count.saturating_add(1);
            }
            let prior_completion = completed.unwrap_or(entry);
            if request.flags.contains(PairFlag::SetPcMark) {
                // `setpcmark()` records the logical cursor before this move.
                let _ = host.access.with_ex_editor(|editor| {
                    editor.set_local_mark(context.buffer, '\'', prior_completion)
                });
                let _ = host.access.with_ex_editor(|editor| {
                    editor.set_local_mark(context.buffer, '`', prior_completion)
                });
            }
            completed = Some(at);
            if !request.flags.contains(PairFlag::Repeat) {
                break;
            }
            nest = 1;
        }
    }
    Ok(PairOutcome { count, completed })
}

// ---------------------------------------------------------------------------
// searchcount()
// ---------------------------------------------------------------------------

/// Parsed `searchcount()` option Dict, with upstream's defaults applied:
/// timeout 40 ms (`SEARCH_STAT_DEF_TIMEOUT`), `maxsearchcount`, and
/// recompute on. Entries parse in upstream's order.
struct SearchCountOptions {
    timeout: i64,
    maxcount: i64,
    recompute: bool,
    pattern: Option<String>,
    pos: Option<(i64, i64, i64)>,
}

/// Parses the optional options Dict; non-Dict values and null Dicts raise
/// E1206, a locked Dict raises E742, and entries follow upstream's order
/// timeout → maxcount → recompute → pattern → pos.
fn parse_search_count_options(
    editor: &Editor,
    arg: Option<&Typval>,
) -> ox_eval::Result<SearchCountOptions> {
    let mut options = SearchCountOptions {
        timeout: 40,
        maxcount: maxsearchcount(editor),
        recompute: true,
        pattern: None,
        pos: None,
    };
    let Some(options_value) = arg else {
        return Ok(options);
    };
    if !matches!(options_value, Typval::Dict(_)) || options_value.is_null_dict() {
        return Err(EvalError::new(
            "E1206",
            0,
            "Dictionary required for argument 1",
        ));
    }
    let Typval::Dict(dict) = options_value else {
        return Err(EvalError::new(
            "E1206",
            0,
            "Dictionary required for argument 1",
        ));
    };
    let Ok(entries) = dict.try_borrow() else {
        return Err(EvalError::new("E742", 0, "Cannot change value"));
    };
    let number_entry = |key: &[u8]| -> ox_eval::Result<Option<i64>> {
        entries.get(key).map(number_value).transpose()
    };
    if let Some(value) = number_entry(b"timeout")? {
        options.timeout = value;
    }
    if let Some(value) = number_entry(b"maxcount")? {
        options.maxcount = value;
    }
    if let Some(value) = number_entry(b"recompute")? {
        options.recompute = value != 0;
    }
    if let Some(value) = entries.get(b"pattern" as &[u8]) {
        options.pattern = Some(input_string_arg(value)?.to_string_lossy().into_owned());
    }
    if let Some(value) = entries.get(b"pos" as &[u8]) {
        options.pos = Some(parse_search_count_pos(value)?);
    }
    Ok(options)
}

/// `searchcount()`: dictionary parse, bounded scan, per-Editor cache, and the
/// upstream result shape (`f_searchcount` + `update_search_stat`).
fn searchcount<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    let options = host
        .access
        .with_ex_editor(|editor| parse_search_count_options(editor, args.first()))?;
    // An explicit empty pattern answers `{}` before anything else is touched,
    // and never rewrites the `/` register.
    if options.pattern.as_deref() == Some("") {
        return Ok(empty_dict());
    }
    let pattern = match options.pattern {
        Some(pattern) => pattern,
        None => host.access.with_ex_editor(|editor| slash_pattern(editor)),
    };
    if pattern.is_empty() {
        // No effective pattern: the empty Dict of a never-defined search.
        return Ok(empty_dict());
    }

    let Some(context) = host.access.with_ex_editor(search_context)? else {
        // Valid arguments but no window: the neutral no-match shape.
        return Ok(count_dict(0, 0, false, 0, options.maxcount));
    };
    let pos = options.pos.unwrap_or((
        i64::try_from(context.cursor.lnum).unwrap_or(i64::MAX),
        i64::try_from(context.cursor.col).unwrap_or(i64::MAX),
        0,
    ));

    // `recompute: 0` with a cached record answers that record unchanged —
    // including its previous `maxcount` — whatever this call passed.
    if !options.recompute
        && let Some(cache) = host
            .access
            .with_ex_editor(|editor| editor.search_count().cloned())
    {
        return Ok(count_dict(
            cache.current,
            cache.total,
            cache.exact_match,
            cache.incomplete,
            cache.maxcount,
        ));
    }

    let magic = if host
        .access
        .with_ex_editor(|editor| option_bool(editor, "magic", true))
    {
        Magic::Magic
    } else {
        Magic::NoMagic
    };
    let text = SearchText::new(&context.lines).map_err(|error| search_error(&error))?;
    let effective_pattern = crate::search::pattern_with_case(
        &pattern,
        host.access
            .with_ex_editor(|editor| option_bool(editor, "ignorecase", false)),
    );
    let prog = crate::search::compile_search(&effective_pattern, magic)
        .map_err(|error| search_error(&error))?;
    let scan = crate::search::scan_count(
        &text,
        &prog,
        pos,
        options.maxcount,
        count_deadline(Some(options.timeout)),
    )
    .map_err(|error| search_error(&error))?;

    // A scan that found at least one candidate is cacheable; a no-match scan
    // follows upstream's "no last position" behavior and caches nothing.
    let record = SearchCountState {
        pattern,
        buffer: context.buffer,
        changedtick: context.changedtick,
        pos: Position {
            lnum: usize::try_from(pos.0).unwrap_or(1).max(1),
            col: usize::try_from(pos.1).unwrap_or(0),
        },
        current: scan.current,
        total: scan.total,
        exact_match: scan.exact_match,
        incomplete: scan.incomplete,
        maxcount: options.maxcount,
    };
    host.access
        .with_ex_editor(|editor| *editor.search_count_mut() = scan.found_any.then_some(record));
    Ok(count_dict(
        scan.current,
        scan.total,
        scan.exact_match,
        scan.incomplete,
        options.maxcount,
    ))
}

/// `'maxsearchcount'`, or upstream's default of 999 when unset.
fn maxsearchcount(editor: &Editor) -> i64 {
    match editor.options().get_global("maxsearchcount") {
        Ok(OptionValue::Number(value)) => *value,
        _ => 999,
    }
}

/// Parses the `pos` entry: a three-item `[lnum, col, off]` list whose column
/// converts from one-based to zero-based (`f_searchcount`).
fn parse_search_count_pos(value: &Typval) -> ox_eval::Result<(i64, i64, i64)> {
    let Typval::List(list) = value else {
        return Err(e475("pos"));
    };
    let Ok(items) = list.try_borrow() else {
        return Err(EvalError::new("E742", 0, "Cannot change value"));
    };
    if items.items.len() != 3 {
        return Err(e475("List format should be [lnum, col, off]"));
    }
    let lnum = number_value(&items.items[0])?;
    let col = number_value(&items.items[1])?.saturating_sub(1);
    let coladd = number_value(&items.items[2])?;
    Ok((lnum, col, coladd))
}

/// The result Dict in upstream insertion order; every value is a Number.
fn count_dict(
    current: i64,
    total: i64,
    exact_match: bool,
    incomplete: i64,
    maxcount: i64,
) -> Typval {
    Typval::dict(vec![
        (OxStr::from("current"), Typval::Number(current)),
        (OxStr::from("total"), Typval::Number(total)),
        (
            OxStr::from("exact_match"),
            Typval::Number(i64::from(exact_match)),
        ),
        (OxStr::from("incomplete"), Typval::Number(incomplete)),
        (OxStr::from("maxcount"), Typval::Number(maxcount)),
    ])
}

/// The empty Dict a `searchcount()` call returns without an effective pattern.
fn empty_dict() -> Typval {
    Typval::dict(Vec::new())
}

/// The pattern of the `/` register, the effective pattern when the call
/// passes none.
fn slash_pattern(editor: &Editor) -> String {
    editor
        .registers()
        .get('/')
        .ok()
        .flatten()
        .map_or_else(String::new, |content| {
            String::from_utf8_lossy(&content.to_bytes()).into_owned()
        })
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Window, buffer, cursor, text, and change-tick snapshot for one scan.
struct SearchContext {
    window: WinHandle,
    buffer: ox_types::BufHandle,
    cursor: Position,
    lines: Vec<Vec<u8>>,
    changedtick: u64,
}

/// Snapshots the current window; `None` when the editor has no window.
fn search_context(editor: &mut Editor) -> ox_eval::Result<Option<SearchContext>> {
    let Some(window) = editor.current_window() else {
        return Ok(None);
    };
    let (buffer, cursor) = {
        let state = editor
            .window(window)
            .map_err(|error| EvalError::new("E117", 0, error.to_string()))?;
        (state.buffer, state.cursor)
    };
    let lines =
        buffer_lines(editor, buffer).map_err(|error| EvalError::new("E117", 0, error.clone()))?;
    let changedtick = changedtick_of(editor, buffer);
    Ok(Some(SearchContext {
        window,
        buffer,
        cursor,
        lines,
        changedtick,
    }))
}

fn changedtick_of(editor: &Editor, buffer: ox_types::BufHandle) -> u64 {
    editor
        .buffer(buffer)
        .map_or(0, crate::buffer::BufferState::script_changedtick)
}

fn place_cursor(editor: &mut Editor, window: WinHandle, at: Position) -> ox_eval::Result<()> {
    editor
        .set_window_cursor(window, at)
        .map_err(|error| EvalError::new("E117", 0, error.to_string()))
}

fn make_scanner(
    text: &crate::search::SearchText,
    direction: SearchDirection,
    stop_line: Option<usize>,
    deadline: Option<Instant>,
    wrap: bool,
) -> CandidateScan<'_> {
    let scanner = CandidateScan::new(text, direction, stop_line, deadline);
    if wrap { scanner.with_wrap() } else { scanner }
}

/// The monotonic deadline for a millisecond bound; nonpositive means no limit.
fn count_deadline(timeout: Option<i64>) -> Option<Instant> {
    timeout
        .filter(|millis| *millis > 0)
        .and_then(|millis| u64::try_from(millis).ok())
        .map(|millis| Instant::now() + Duration::from_millis(millis))
}

/// Parses a pair stopline or timeout with Vim numeric coercion. Negative
/// bounds are invalid and zero means no bound.
fn parse_pair_bound(value: Option<&Typval>) -> ox_eval::Result<Option<i64>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let bound = number_value(value)?;
    if bound < 0 {
        return Err(e475(&bound.to_string()));
    }
    Ok((bound != 0).then_some(bound))
}

/// `tv_get_string_buf_chk` for the pattern-token arguments.
fn pattern_arg(value: &Typval) -> ox_eval::Result<String> {
    Ok(input_string_arg(value)?.to_string_lossy().into_owned())
}

fn option_bool(editor: &Editor, name: &str, fallback: bool) -> bool {
    match editor.options().get_global(name) {
        Ok(OptionValue::Boolean(value)) => *value,
        _ => fallback,
    }
}

/// `E475: Invalid argument: {argument}`.
fn e475(argument: &str) -> EvalError {
    EvalError::new("E475", 0, format!("Invalid argument: {argument}"))
}

/// Engine failures surface under the same mapping `search()` has always used.
fn search_error(error: &SearchError) -> EvalError {
    EvalError::new("E486", 0, error.to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use ox_text::Buffer;
    use ox_types::Funcref;

    use super::*;
    use crate::TestEditorAccess;
    use crate::excmd_exec::ExecError;
    use crate::{ExExecutor, Geometry, VimExceptionKind};

    /// One listed buffer holding `text`, shown in an 80x24 window.
    fn editor_with(text: &str) -> TestEditorAccess {
        let editor = TestEditorAccess::new(Editor::new());
        {
            let mut e = editor.editor_mut();
            let buffer = e
                .create_buffer_with(Buffer::from_bytes(text.as_bytes()).unwrap(), true)
                .unwrap();
            e.create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
                .unwrap();
        }
        editor
    }

    /// Runs `script` against `text` and answers numeric globals.
    fn numbers(text: &str, script: &str, names: &[&str]) -> Vec<i64> {
        let editor = editor_with(text);
        let mut exec = ExExecutor::new();
        exec.execute_script(&editor, "search.vim", script).unwrap();
        names
            .iter()
            .map(|name| {
                match exec
                    .scope()
                    .get_scoped(ox_eval::ScopeKind::Global, name.as_bytes(), 0)
                    .unwrap_or_else(|error| panic!("no g:{name}: {error:?}"))
                {
                    Typval::Number(value) => *value,
                    other => panic!("expected a Number in g:{name}, got {other:?}"),
                }
            })
            .collect()
    }

    /// The Vim error code `script` raises against `text`.
    fn error_code(text: &str, script: &str) -> String {
        let editor = editor_with(text);
        let mut exec = ExExecutor::new();
        match exec.execute_script(&editor, "search.vim", script) {
            Err(ExecError::Vim(exception)) => match exception.kind {
                VimExceptionKind::Error(code) => code,
                VimExceptionKind::Throw => panic!("expected an error exception, got Throw"),
            },
            other => panic!("expected a Vim error, got {other:?}"),
        }
    }

    /// The string value of `@/` after running `script`.
    fn slash_register(text: &str, script: &str) -> String {
        let editor = editor_with(text);
        let mut exec = ExExecutor::new();
        exec.execute_script(&editor, "search.vim", script).unwrap();
        editor
            .editor()
            .registers()
            .get('/')
            .ok()
            .flatten()
            .map_or(String::new(), |content| {
                String::from_utf8_lossy(&content.to_bytes()).into_owned()
            })
    }

    // Finding: the pair-bound parser exists, coerces through Vim numeric
    // rules, rejects negatives with E475, and maps zero to unbounded.
    #[test]
    fn pair_bounds_coerce_like_vim_numbers() {
        assert_eq!(parse_pair_bound(None).unwrap(), None);
        assert_eq!(parse_pair_bound(Some(&Typval::Number(0))).unwrap(), None);
        assert_eq!(parse_pair_bound(Some(&Typval::Number(9))).unwrap(), Some(9));
        assert_eq!(
            parse_pair_bound(Some(&Typval::String(OxStr::from("5")))).unwrap(),
            Some(5)
        );
        assert_eq!(
            parse_pair_bound(Some(&Typval::Number(-99)))
                .unwrap_err()
                .code,
            "E475"
        );
    }

    // Finding: end to end, a negative stopline is E475 while zero stays
    // unbounded and the pair completes.
    #[test]
    fn searchpair_rejects_negative_bounds_and_treats_zero_as_unbounded() {
        let code = error_code(
            "x(y)x",
            "call cursor(1, 4)\ncall searchpair('(', '', ')', 'b', '', -1)",
        );
        assert_eq!(code, "E475");
        let values = numbers(
            "x(y)x",
            "call cursor(1, 4)\nlet g:a = searchpair('(', '', ')', 'b', '', 0, 0)",
            &["a"],
        );
        assert_eq!(values, vec![1]);
    }

    #[test]
    fn no_move_pair_search_restores_entry_after_repeating_skip() {
        let values = numbers(
            "start\nend\nstart\nend",
            "call cursor(1, 1)\n\
             let g:count = searchpair('start', '', 'end', 'nrm', '0+0')\n\
             let g:line = line('.')",
            &["count", "line"],
        );
        assert_eq!(values, vec![1, 1]);
    }

    // Finding: skip arguments coerce through Vim string rules — Lists and
    // Dicts are type errors, while strings stay expression inputs.
    #[test]
    fn skip_arguments_coerce_like_vim_types() {
        assert!(matches!(parse_skip(None).unwrap(), Skip::None));
        assert!(matches!(
            parse_skip(Some(&Typval::Number(0))).unwrap(),
            Skip::None
        ));
        assert!(matches!(
            parse_skip(Some(&Typval::String(OxStr::from("")))).unwrap(),
            Skip::None
        ));
        assert!(matches!(
            parse_skip(Some(&Typval::String(OxStr::from("col('.')>1")))).unwrap(),
            Skip::Expression(_)
        ));
        let callable = Typval::Funcref(Funcref {
            name: OxStr::from("SkipIt"),
            args: Vec::new(),
            dict: None,
            registry: None,
        });
        assert!(matches!(
            parse_skip(Some(&callable)).unwrap(),
            Skip::Callable(_)
        ));
        assert_eq!(
            parse_skip(Some(&Typval::list(Vec::new())))
                .unwrap_err()
                .code,
            "E730"
        );
        assert_eq!(
            parse_skip(Some(&Typval::dict(Vec::new())))
                .unwrap_err()
                .code,
            "E731"
        );
    }

    // Finding: callable skips still invoke, expression skips evaluate, and a
    // truthy verdict skips candidates while a falsy one participates.
    #[test]
    fn searchpair_evaluates_skips_through_the_callable_seam() {
        let values = numbers(
            "x(y)x",
            "function! SkipAll()\n  return 1\nendfunction\n\
             function! SkipNone()\n  return 0\nendfunction\n\
             call cursor(1, 2)\n\
             let g:a = searchpair('(', '', ')', '', function('SkipAll'))\n\
             let g:b = searchpair('(', '', ')', '', function('SkipNone'))\n\
             call cursor(1, 2)\n\
             let g:c = searchpair('(', '', ')', '', 'SkipNone()')",
            &["a", "b", "c"],
        );
        assert_eq!(values, vec![0, 1, 1]);
    }

    // Finding: Lists and Dicts as skip arguments raise the Vim type errors.
    #[test]
    fn searchpair_rejects_list_and_dict_skips() {
        assert_eq!(
            error_code(
                "x(y)x",
                "call cursor(1, 2)\ncall searchpair('(', '', ')', '', [0])",
            ),
            "E730"
        );
        assert_eq!(
            error_code(
                "x(y)x",
                "call cursor(1, 2)\ncall searchpair('(', '', ')', '', {})",
            ),
            "E731"
        );
    }

    // Finding: user capture groups inside {start} must not demote the {end}
    // token to Middle — the pair completes instead of returning 0.
    #[test]
    fn searchpair_classifies_branches_with_user_captures() {
        let values = numbers(
            "aXb",
            "call cursor(1, 2)\nlet g:a = searchpair('\\(a\\)', '', 'b')",
            &["a"],
        );
        assert_eq!(values, vec![1]);
    }

    // Finding: pair matching honors effective 'ignorecase' end to end.
    #[test]
    fn searchpair_honors_effective_ignorecase() {
        let values = numbers(
            "FOO bar BAR",
            "set ignorecase\ncall cursor(1, 5)\nlet g:a = searchpair('foo', '', 'bar')",
            &["a"],
        );
        assert_eq!(values, vec![1]);
    }

    // Finding: searchcount decodes pattern and pos after recompute, in
    // source order, and an explicit pattern positions the scan.
    #[test]
    fn searchcount_parses_explicit_pattern_and_pos() {
        let values = numbers(
            "foo\nfoo\nfoo",
            "call cursor(1, 1)\n\
             let g:a = searchcount({'pattern': 'foo', 'pos': [3, 1, 0]})\n\
             let g:c = g:a.current\nlet g:t = g:a.total\nlet g:e = g:a.exact_match",
            &["c", "t", "e"],
        );
        assert_eq!(values, vec![3, 3, 1]);
    }

    // Finding: pattern errors surface before pos errors (source order).
    #[test]
    fn searchcount_reports_pattern_errors_before_pos_errors() {
        assert_eq!(
            error_code("foo", "call searchcount({'pattern': [], 'pos': 5})"),
            "E730"
        );
        assert_eq!(
            error_code("foo", "call searchcount({'pattern': 'foo', 'pos': [1, 1]})"),
            "E475"
        );
    }

    // Finding: an explicit empty pattern short-circuits to {} and never
    // touches the `/` register.
    #[test]
    fn searchcount_empty_pattern_short_circuits_and_keeps_slash() {
        let values = numbers(
            "foo",
            "let @/ = 'foo'\n\
             let g:e = empty(searchcount({'pattern': ''}))\n\
             let g:r = searchcount({'pattern': 'foo'})\n\
             let g:s = g:r.total",
            &["e", "s"],
        );
        assert_eq!(values, vec![1, 1]);
        assert_eq!(
            slash_register("foo", "let @/ = 'foo'\ncall searchcount({'pattern': ''})"),
            "foo"
        );
    }

    // Finding: count scans honor effective 'ignorecase' and explicit \C.
    #[test]
    fn searchcount_honors_effective_ignorecase_and_explicit_case() {
        let values = numbers(
            "Foo",
            "set ignorecase\n\
             let g:ra = searchcount({'pattern': 'foo'})\n\
             let g:a = g:ra.total\n\
             let g:rb = searchcount({'pattern': 'foo\\C'})\n\
             let g:b = g:rb.total",
            &["a", "b"],
        );
        assert_eq!(values, vec![1, 0]);
    }

    // Finding: repeated completions advance the previous-context marks to
    // the position before the final completed move. `r` keeps the scan
    // running past the first completed pair (SP_REPEAT); `s` records the
    // previous-context mark at every completion (`do_searchpair` breaks
    // after the first completion without it, leaving the mark at the
    // entry position).
    #[test]
    fn repeated_completions_advance_the_previous_context_mark() {
        let editor = editor_with("x ( ) ) ( ) )");
        let mut exec = ExExecutor::new();
        exec.execute_script(
            &editor,
            "mark.vim",
            "call cursor(1, 1)\ncall searchpair('(', '', ')', 'srm')",
        )
        .unwrap();
        let window = editor.editor().current_window().unwrap();
        let buffer = editor.editor().window(window).unwrap().buffer;
        let mark = editor.editor().local_mark(buffer, '\'').unwrap().unwrap();
        assert_eq!(mark, Position { lnum: 1, col: 6 });
        assert_eq!(
            editor.editor().local_mark(buffer, '`').unwrap().unwrap(),
            mark
        );
    }

    // A single completion still records the entry position.
    #[test]
    fn first_completion_marks_the_entry_position() {
        let editor = editor_with("( )");
        let mut exec = ExExecutor::new();
        exec.execute_script(
            &editor,
            "mark.vim",
            "call cursor(1, 1)\ncall searchpair('(', '', ')', 's')",
        )
        .unwrap();
        let window = editor.editor().current_window().unwrap();
        let buffer = editor.editor().window(window).unwrap().buffer;
        let mark = editor.editor().local_mark(buffer, '\'').unwrap().unwrap();
        assert_eq!(mark, Position { lnum: 1, col: 0 });
    }
}
