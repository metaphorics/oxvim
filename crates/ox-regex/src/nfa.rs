use std::collections::BTreeSet;

use crate::bt::{self, State};
use crate::parser::{CharClass, Expr};
use crate::{ExecError, Prog, Text};

#[derive(Clone, Debug)]
enum Inst {
    Char(char),
    Any { newline: bool },
    Class(CharClass),
    SaveStart(usize),
    SaveEnd(usize),
    SetStart,
    SetEnd,
    Split(usize, usize),
    Jump(usize),
    Fallback(Expr),
    Match,
}

pub(crate) fn search(prog: &Prog, text: &Text, from: usize) -> Result<Option<State>, ExecError> {
    let mut code = Vec::new();
    compile_expr(&prog.expr, &mut code);
    code.push(Inst::Match);
    let mut steps = 0;
    for candidate in candidate_offsets(text.as_str(), from) {
        if let Some(mut state) = run_candidate(prog, text, &code, candidate, &mut steps)? {
            state.set_search_start(candidate);
            return Ok(Some(state));
        }
    }
    Ok(None)
}

fn run_candidate(
    prog: &Prog,
    text: &Text,
    code: &[Inst],
    candidate: usize,
    steps: &mut usize,
) -> Result<Option<State>, ExecError> {
    let mut stack = vec![(0, State::new(candidate, prog.capture_count), BTreeSet::new())];
    while let Some((mut pc, mut state, mut visited)) = stack.pop() {
        loop {
            *steps = steps.checked_add(1).ok_or(ExecError::StepLimit)?;
            if *steps > prog.step_limit {
                return Err(ExecError::StepLimit);
            }
            if !visited.insert((pc, state.pos)) {
                break;
            }
            let Some(inst) = code.get(pc) else {
                break;
            };
            match inst {
                Inst::Char(expected) => {
                    if let Some((actual, next)) = next_char(text.as_str(), state.pos) {
                        if bt::chars_equal(*expected, actual, prog.ignore_case) {
                            state.pos = next;
                            pc += 1;
                            continue;
                        }
                    }
                    break;
                }
                Inst::Any { newline } => {
                    if let Some((actual, next)) = next_char(text.as_str(), state.pos) {
                        if *newline || actual != '\n' {
                            state.pos = next;
                            pc += 1;
                            continue;
                        }
                    }
                    break;
                }
                Inst::Class(class) => {
                    if let Some((actual, next)) = next_char(text.as_str(), state.pos) {
                        if bt::class_matches(class, actual, prog.ignore_case) {
                            state.pos = next;
                            pc += 1;
                            continue;
                        }
                    }
                    break;
                }
                Inst::SaveStart(index) => {
                    state.open_capture(*index);
                    pc += 1;
                }
                Inst::SaveEnd(index) => {
                    state.close_capture(*index);
                    pc += 1;
                }
                Inst::SetStart => {
                    state.set_match_start();
                    pc += 1;
                }
                Inst::SetEnd => {
                    state.set_match_end();
                    pc += 1;
                }
                Inst::Jump(target) => pc = *target,
                Inst::Split(first, second) => {
                    stack.push((*second, state.clone(), visited.clone()));
                    pc = *first;
                }
                Inst::Fallback(expr) => {
                    let results = bt::match_at(prog, text, expr, state, candidate)?;
                    for result in results.into_iter().rev() {
                        stack.push((pc + 1, result, visited.clone()));
                    }
                    break;
                }
                Inst::Match => return Ok(Some(state)),
            }
        }
    }
    Ok(None)
}

fn compile_expr(expr: &Expr, code: &mut Vec<Inst>) {
    match expr {
        Expr::Empty => {}
        Expr::Literal(ch) => code.push(Inst::Char(*ch)),
        Expr::Any { newline } => code.push(Inst::Any { newline: *newline }),
        Expr::Class(class) => code.push(Inst::Class(class.clone())),
        Expr::Concat(parts) => {
            for part in parts {
                compile_expr(part, code);
            }
        }
        Expr::Alt(branches) => compile_alt(branches, code),
        Expr::Repeat { expr: _, min, max: Some(max), greedy: _ }
            if (*max > 500 || max.saturating_sub(*min) > 200) && *min < 200 =>
        {
            code.push(Inst::Fallback(expr.clone()));
        }
        Expr::Repeat { expr, min, max, greedy } => compile_repeat(expr, *min, *max, *greedy, code),
        Expr::Group { index, expr } => {
            if let Some(index) = index {
                code.push(Inst::SaveStart(*index));
            }
            compile_expr(expr, code);
            if let Some(index) = index {
                code.push(Inst::SaveEnd(*index));
            }
        }
        Expr::OptionalSeq(parts) => {
            let branches = (0..=parts.len())
                .rev()
                .map(|length| Expr::Concat(parts[..length].to_vec()))
                .collect::<Vec<_>>();
            compile_alt(&branches, code);
        }
        Expr::SetStart => code.push(Inst::SetStart),
        Expr::SetEnd => code.push(Inst::SetEnd),
        Expr::Anchor(_) | Expr::Look { .. } | Expr::Backref(_) | Expr::And(_) => {
            code.push(Inst::Fallback(expr.clone()));
        }
    }
}

fn compile_alt(branches: &[Expr], code: &mut Vec<Inst>) {
    let Some((last, leading)) = branches.split_last() else {
        return;
    };
    let mut end_jumps = Vec::new();
    for branch in leading {
        let split = code.len();
        code.push(Inst::Split(split + 1, 0));
        compile_expr(branch, code);
        let jump = code.len();
        code.push(Inst::Jump(0));
        end_jumps.push(jump);
        let next = code.len();
        if let Inst::Split(_, second) = &mut code[split] {
            *second = next;
        }
    }
    compile_expr(last, code);
    let end = code.len();
    for jump in end_jumps {
        code[jump] = Inst::Jump(end);
    }
}

fn compile_repeat(expr: &Expr, min: usize, max: Option<usize>, greedy: bool, code: &mut Vec<Inst>) {
    for _ in 0..min {
        compile_expr(expr, code);
    }
    match max {
        Some(maximum) => {
            for _ in min..maximum {
                compile_optional(expr, greedy, code);
            }
        }
        None => {
            let split = code.len();
            code.push(Inst::Split(0, 0));
            let body = code.len();
            compile_expr(expr, code);
            code.push(Inst::Jump(split));
            let end = code.len();
            code[split] = if greedy { Inst::Split(body, end) } else { Inst::Split(end, body) };
        }
    }
}

fn compile_optional(expr: &Expr, greedy: bool, code: &mut Vec<Inst>) {
    let split = code.len();
    code.push(Inst::Split(0, 0));
    let body = code.len();
    compile_expr(expr, code);
    let end = code.len();
    code[split] = if greedy { Inst::Split(body, end) } else { Inst::Split(end, body) };
}

fn next_char(text: &str, offset: usize) -> Option<(char, usize)> {
    let ch = text.get(offset..)?.chars().next()?;
    Some((ch, offset + ch.len_utf8()))
}

fn candidate_offsets(text: &str, from: usize) -> impl Iterator<Item = usize> + '_ {
    text.char_indices()
        .map(|(offset, _)| offset)
        .chain(std::iter::once(text.len()))
        .filter(move |offset| *offset >= from)
}
