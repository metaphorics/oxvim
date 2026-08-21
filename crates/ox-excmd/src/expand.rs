//! Structured scanning of command-line-special placeholders.

/// A command-line-special value whose resolution belongs to the editor host.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CmdlineSpecial {
    /// `%`: current file name.
    CurrentFile,
    /// `#`: alternate file name.
    AlternateFile,
    /// `<cword>`: word under the cursor.
    CurrentWord,
    /// `<cWORD>`: whitespace-delimited word under the cursor.
    CurrentBigWord,
    /// `<cfile>`: file name under the cursor.
    CurrentFileUnderCursor,
    /// `<cexpr>`: expression under the cursor.
    CurrentExpression,
    /// `<afile>`: autocommand file name.
    AutocmdFile,
    /// `<abuf>`: autocommand buffer number.
    AutocmdBuffer,
    /// `<amatch>`: autocommand match.
    AutocmdMatch,
    /// `<sfile>`: sourced script file.
    ScriptFile,
    /// `<slnum>`: sourced script line number.
    ScriptLine,
    /// `<stack>`: script call stack.
    ScriptStack,
    /// `<script>`: defining script file name.
    ScriptDefinition,
    /// `<sflnum>`: sourced script file line number.
    ScriptFileLine,
    /// `<SID>`: current script ID prefix.
    ScriptId,
}

/// One structured segment of a scanned argument.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExpansionPart {
    /// Literal text, including escaped specials.
    Literal {
        /// Decoded literal text.
        text: String,
        /// Byte range in the original input.
        span: std::ops::Range<usize>,
    },
    /// A value that must be resolved by the editor host.
    Placeholder {
        /// Placeholder kind.
        special: CmdlineSpecial,
        /// Byte range in the original input.
        span: std::ops::Range<usize>,
    },
}

/// Host seam for resolving editor-dependent command-line-special values.
pub trait CmdlineContext {
    /// Resolves one placeholder. `None` leaves it unresolved.
    fn resolve(&self, special: CmdlineSpecial) -> Option<String>;
}

/// Scans a command argument without consulting editor state.
///
/// A backslash before `%`, `#`, or a recognized angle token makes that token
/// literal. `<lt>` is the literal less-than escape and therefore never calls
/// the host.
#[must_use]
pub fn scan_expansions(input: &str) -> Vec<ExpansionPart> {
    let mut parts = Vec::new();
    let mut literal = String::new();
    let mut literal_start = 0;
    let mut cursor = 0;

    while cursor < input.len() {
        let escaped = input.as_bytes()[cursor] == b'\\';
        let token_start = if escaped { cursor + 1 } else { cursor };
        let Some((token_len, token)) = expansion_at(input, token_start) else {
            let character = input[cursor..].chars().next();
            let Some(character) = character else {
                break;
            };
            literal.push(character);
            cursor += character.len_utf8();
            continue;
        };

        if escaped {
            literal.push_str(&input[token_start..token_start + token_len]);
            cursor = token_start + token_len;
            continue;
        }

        push_literal(&mut parts, &mut literal, literal_start, cursor);
        if token == ExpansionToken::LessThan {
            parts.push(ExpansionPart::Literal {
                text: "<".to_owned(),
                span: cursor..cursor + token_len,
            });
        } else if let ExpansionToken::Special(special) = token {
            parts.push(ExpansionPart::Placeholder {
                special,
                span: cursor..cursor + token_len,
            });
        }
        cursor += token_len;
        literal_start = cursor;
    }
    push_literal(&mut parts, &mut literal, literal_start, input.len());
    parts
}

/// Resolves scanned parts through a host, preserving unresolved source tokens.
#[must_use]
pub fn expand_with<C: CmdlineContext + ?Sized>(input: &str, context: &C) -> String {
    let mut output = String::new();
    for part in scan_expansions(input) {
        match part {
            ExpansionPart::Literal { text, .. } => output.push_str(&text),
            ExpansionPart::Placeholder { special, span } => {
                if let Some(value) = context.resolve(special) {
                    output.push_str(&value);
                } else {
                    output.push_str(&input[span]);
                }
            }
        }
    }
    output
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExpansionToken {
    Special(CmdlineSpecial),
    LessThan,
}

fn expansion_at(input: &str, start: usize) -> Option<(usize, ExpansionToken)> {
    let tail = input.get(start..)?;
    if tail.starts_with('%') {
        return Some((1, ExpansionToken::Special(CmdlineSpecial::CurrentFile)));
    }
    if tail.starts_with('#') {
        return Some((1, ExpansionToken::Special(CmdlineSpecial::AlternateFile)));
    }
    const ANGLE: &[(&str, CmdlineSpecial)] = &[
        ("<cword>", CmdlineSpecial::CurrentWord),
        ("<cWORD>", CmdlineSpecial::CurrentBigWord),
        ("<cfile>", CmdlineSpecial::CurrentFileUnderCursor),
        ("<cexpr>", CmdlineSpecial::CurrentExpression),
        ("<afile>", CmdlineSpecial::AutocmdFile),
        ("<abuf>", CmdlineSpecial::AutocmdBuffer),
        ("<amatch>", CmdlineSpecial::AutocmdMatch),
        ("<sfile>", CmdlineSpecial::ScriptFile),
        ("<slnum>", CmdlineSpecial::ScriptLine),
        ("<stack>", CmdlineSpecial::ScriptStack),
        ("<script>", CmdlineSpecial::ScriptDefinition),
        ("<sflnum>", CmdlineSpecial::ScriptFileLine),
        ("<SID>", CmdlineSpecial::ScriptId),
    ];
    if tail.starts_with("<lt>") {
        return Some((4, ExpansionToken::LessThan));
    }
    ANGLE.iter().find_map(|(name, special)| {
        tail.starts_with(name)
            .then_some((name.len(), ExpansionToken::Special(*special)))
    })
}

fn push_literal(
    parts: &mut Vec<ExpansionPart>,
    literal: &mut String,
    start: usize,
    end: usize,
) {
    if literal.is_empty() {
        return;
    }
    parts.push(ExpansionPart::Literal {
        text: std::mem::take(literal),
        span: start..end,
    });
}
