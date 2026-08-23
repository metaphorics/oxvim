//! Fuzzy matching for `matchfuzzy()` and `matchfuzzypos()`.
//!
//! Direct port of `src/nvim/fuzzy.c`: the fzy scoring core (`has_match`,
//! `setup_match_struct`, `match_row`, `match_positions`), the per-word driver
//! `fuzzy_match`, and the list walker `fuzzy_match_in_list`. Every constant
//! and the score-to-integer conversion are upstream's; no scoring rule is
//! invented here.
//!
//! Positions and lengths are counted in composed characters, matching
//! upstream's `utf_ptr2char` + `MB_PTR_ADV` walk, which reads one base
//! codepoint and then skips its composing marks.

/// `FUZZY_MATCH_MAX_LEN` (`fuzzy.h:12`): most characters that can be matched.
pub(crate) const MATCH_MAX_LEN: usize = 1024;

/// `FUZZY_SCORE_NONE` (`fuzzy.h:13`): `INT_MIN`, an invalid fuzzy score.
const SCORE_NONE: i32 = i32::MIN;

// fzy weights, `fuzzy.c:740-747`.
const SCORE_GAP_LEADING: f64 = -0.005;
const SCORE_GAP_TRAILING: f64 = -0.005;
const SCORE_GAP_INNER: f64 = -0.01;
const SCORE_MATCH_CONSECUTIVE: f64 = 1.0;
const SCORE_MATCH_SLASH: f64 = 0.9;
const SCORE_MATCH_WORD: f64 = 0.8;
const SCORE_MATCH_CAPITAL: f64 = 0.7;
const SCORE_MATCH_DOT: f64 = 0.6;
const SCORE_SCALE: f64 = 1000.0;

/// One fuzzy match: the accumulated score and the matched character
/// positions, one per non-blank pattern character.
pub(crate) struct FuzzyMatch {
    /// `*outScore` from `fuzzy_match`, a C `int`.
    pub(crate) score: i32,
    /// `matches[0..numMatches]`, character indices into the haystack.
    pub(crate) positions: Vec<usize>,
}

/// Split `text` into composed characters: a base codepoint followed by its
/// zero-width composing marks, as `MB_PTR_ADV` does. Invalid UTF-8 bytes
/// decode to `U+FFFD` through `from_utf8_lossy`, so no input can panic.
pub(crate) fn composed_chars(text: &[u8]) -> Vec<char> {
    let decoded = String::from_utf8_lossy(text);
    let mut characters = Vec::new();
    for character in decoded.chars() {
        if is_composing(character) && !characters.is_empty() {
            continue;
        }
        characters.push(character);
    }
    characters
}

/// `utf_iscomposing`: a mark that `MB_PTR_ADV` folds into the previous
/// character. This crate identifies those by zero display width, the same
/// rule `strchars()`/`strcharlen()` already use.
fn is_composing(character: char) -> bool {
    unicode_width::UnicodeWidthChar::width(character).unwrap_or(1) == 0
}

/// `ascii_iswhite`: space or tab, the pattern word separator.
fn is_white(character: char) -> bool {
    character == ' ' || character == '\t'
}

/// `mb_tolower` under the default `'casemap'` (`internal,keepascii`): ASCII
/// stays ASCII, everything else uses the simple (single-character) Unicode
/// lowercase mapping, like `utf8proc_tolower`.
fn to_lower(character: char) -> char {
    if character.is_ascii() {
        return character.to_ascii_lowercase();
    }
    let mut mapped = character.to_lowercase();
    let first = mapped.next().unwrap_or(character);
    if mapped.next().is_none() {
        first
    } else if character == '\u{0130}' {
        'i'
    } else {
        character
    }
}

/// `mb_toupper` under the default `'casemap'`; see [`to_lower`].
fn to_upper(character: char) -> char {
    if character.is_ascii() {
        return character.to_ascii_uppercase();
    }
    let mut mapped = character.to_uppercase();
    let first = mapped.next().unwrap_or(character);
    if mapped.next().is_none() { first } else { character }
}

/// `mb_islower`: `mb_toupper(c) != c`.
fn is_lower(character: char) -> bool {
    to_upper(character) != character
}

/// `mb_isupper`: `mb_tolower(c) != c`.
fn is_upper(character: char) -> bool {
    to_lower(character) != character
}

/// Codepoint ranges that `utf_class_tab` (`mbyte.c`) classifies as blank (0)
/// or punctuation (1); every other codepoint at or above `0x100` is a word
/// character. Adjacent upstream intervals are merged because only the
/// `>= 2` test matters here.
const NON_WORD_INTERVALS: [(u32, u32); 50] = [
    (0x037e, 0x037e), (0x0387, 0x0387), (0x055a, 0x055f), (0x0589, 0x0589),
    (0x05be, 0x05be), (0x05c0, 0x05c0), (0x05c3, 0x05c3), (0x05f3, 0x05f4),
    (0x060c, 0x060c), (0x061b, 0x061b), (0x061f, 0x061f), (0x066a, 0x066d),
    (0x06d4, 0x06d4), (0x0700, 0x070d), (0x0964, 0x0965), (0x0970, 0x0970),
    (0x0df4, 0x0df4), (0x0e4f, 0x0e4f), (0x0e5a, 0x0e5b), (0x0f04, 0x0f12),
    (0x0f3a, 0x0f3d), (0x0f85, 0x0f85), (0x104a, 0x104f), (0x10fb, 0x10fb),
    (0x1361, 0x1368), (0x166d, 0x166e), (0x1680, 0x1680), (0x169b, 0x169c),
    (0x16eb, 0x16ed), (0x1735, 0x1736), (0x17d4, 0x17dc), (0x1800, 0x180a),
    (0x2000, 0x206f), (0x20a0, 0x27ff), (0x2900, 0x2998), (0x29d8, 0x29db),
    (0x29fc, 0x29fd), (0x2e00, 0x2e7f), (0x3000, 0x3020), (0x3030, 0x3030),
    (0x303d, 0x303d), (0xfd3e, 0xfd3f), (0xfe30, 0xfe6b), (0xff00, 0xff0f),
    (0xff1a, 0xff20), (0xff3b, 0xff40), (0xff5b, 0xff65), (0x1d000, 0x1d24f),
    (0x1d400, 0x1d7ff), (0x1f000, 0x1f9ff),
];

/// `vim_iswordc` under the default `'iskeyword'` (`@,48-57,_,192-255`):
/// below `0x100` that is an ASCII letter or digit, `_`, or `0xc0..=0xff`;
/// at or above `0x100` it is `utf_class(c) >= 2`, computed from
/// [`NON_WORD_INTERVALS`].
///
/// Upstream rescues an `Extended_Pictographic` or `Regional_Indicator`
/// codepoint
/// inside a punctuation interval as class 3. That utf8proc property is not
/// available here, so an emoji that upstream would treat as a word character
/// is treated as punctuation; it can only cost such a character its
/// separator bonus.
fn is_word_char(character: char) -> bool {
    let code = u32::from(character);
    if code < 0x100 {
        return character.is_ascii_alphanumeric() || character == '_' || (0xc0..=0xff).contains(&code);
    }
    !NON_WORD_INTERVALS.iter().any(|(first, last)| (*first..=*last).contains(&code))
}

/// `compute_bonus_codepoint` (`fuzzy.c:794-811`).
fn compute_bonus(last: char, current: char) -> f64 {
    if current.is_ascii_alphanumeric() || is_word_char(current) {
        if last == '/' {
            return SCORE_MATCH_SLASH;
        }
        if last == '-' || last == '_' || last == ' ' {
            return SCORE_MATCH_WORD;
        }
        if last == '.' {
            return SCORE_MATCH_DOT;
        }
        if is_upper(current) && is_lower(last) {
            return SCORE_MATCH_CAPITAL;
        }
    }
    0.0
}

/// `has_match` (`fuzzy.c:749-780`): every needle character appears in order,
/// comparing the needle character itself or its uppercase form.
fn has_match(needle: &[char], haystack: &[char]) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut position = 0;
    for wanted in needle {
        let upper = to_upper(*wanted);
        let mut found = false;
        while position < haystack.len() {
            let candidate = haystack[position];
            position += 1;
            if candidate == *wanted || candidate == upper {
                found = true;
                break;
            }
        }
        if !found {
            return false;
        }
    }
    true
}

/// `match_struct` (`fuzzy.c:782-788`) built by `setup_match_struct`.
struct MatchStruct {
    lower_needle: Vec<char>,
    lower_haystack: Vec<char>,
    match_bonus: Vec<f64>,
}

/// `setup_match_struct` (`fuzzy.c:813-837`).
fn setup_match_struct(needle: &[char], haystack: &[char]) -> MatchStruct {
    let lower_needle = needle.iter().take(MATCH_MAX_LEN).map(|character| to_lower(*character)).collect();
    let mut lower_haystack = Vec::new();
    let mut match_bonus = Vec::new();
    let mut previous = '/';
    for character in haystack.iter().take(MATCH_MAX_LEN) {
        lower_haystack.push(to_lower(*character));
        match_bonus.push(compute_bonus(previous, *character));
        previous = *character;
    }
    MatchStruct { lower_needle, lower_haystack, match_bonus }
}

/// `j * SCORE_GAP_LEADING` (`fuzzy.c:860`). A column index is bounded by
/// [`MATCH_MAX_LEN`], so the `u16` conversion is exact.
fn leading_gap(column: usize) -> f64 {
    f64::from(u16::try_from(column).unwrap_or(u16::MAX)) * SCORE_GAP_LEADING
}

/// `match_row` (`fuzzy.c:839-877`).
fn match_row(
    match_data: &MatchStruct,
    row: usize,
    current_d: &mut [f64],
    current_m: &mut [f64],
    last_d: &[f64],
    last_m: &[f64],
) {
    let needle_len = match_data.lower_needle.len();
    let haystack_len = match_data.lower_haystack.len();
    let mut previous_score = f64::NEG_INFINITY;
    let gap_score = if row == needle_len - 1 { SCORE_GAP_TRAILING } else { SCORE_GAP_INNER };
    let mut previous_m = f64::NEG_INFINITY;
    let mut previous_d = f64::NEG_INFINITY;

    for column in 0..haystack_len {
        if match_data.lower_needle[row] == match_data.lower_haystack[column] {
            let mut score = f64::NEG_INFINITY;
            if row == 0 {
                score = leading_gap(column) + match_data.match_bonus[column];
            } else if column > 0 {
                score = (previous_m + match_data.match_bonus[column])
                    .max(previous_d + SCORE_MATCH_CONSECUTIVE);
            }
            previous_d = last_d[column];
            previous_m = last_m[column];
            current_d[column] = score;
            previous_score = score.max(previous_score + gap_score);
            current_m[column] = previous_score;
        } else {
            previous_d = last_d[column];
            previous_m = last_m[column];
            current_d[column] = f64::NEG_INFINITY;
            previous_score += gap_score;
            current_m[column] = previous_score;
        }
    }
}

/// `match_positions` (`fuzzy.c:879-966`): the fzy score, plus the character
/// positions of the optimal match written into `positions`.
fn match_positions(needle: &[char], haystack: &[char], positions: &mut [usize]) -> f64 {
    if needle.is_empty() {
        return f64::NEG_INFINITY;
    }
    let match_data = setup_match_struct(needle, haystack);
    let rows = match_data.lower_needle.len();
    let columns = match_data.lower_haystack.len();

    if columns > MATCH_MAX_LEN || rows > columns {
        return f64::NEG_INFINITY;
    }
    if rows == columns && match_data.lower_needle == match_data.lower_haystack {
        // Equal ignoring case: the shortcut in `fuzzy.c:895-916`.
        for (index, slot) in positions.iter_mut().enumerate().take(rows) {
            *slot = index;
        }
        return f64::INFINITY;
    }

    let mut best_match = vec![f64::NEG_INFINITY; rows * columns];
    let mut best_end = vec![f64::NEG_INFINITY; rows * columns];
    {
        let (row_d, row_m) = (&mut best_end[..columns], &mut best_match[..columns]);
        let seed_d = vec![f64::NEG_INFINITY; columns];
        let seed_m = seed_d.clone();
        match_row(&match_data, 0, row_d, row_m, &seed_d, &seed_m);
    }
    for row in 1..rows {
        let (previous_d, current_d) = best_end.split_at_mut(row * columns);
        let (previous_m, current_m) = best_match.split_at_mut(row * columns);
        let last_d = &previous_d[(row - 1) * columns..][..columns];
        let last_m = &previous_m[(row - 1) * columns..][..columns];
        match_row(&match_data, row, &mut current_d[..columns], &mut current_m[..columns], last_d, last_m);
    }

    // Backtrace, `fuzzy.c:937-960`. `remaining` is the exclusive upper bound
    // on the columns still available, mirroring upstream's `j` walking down
    // once per examined column whether or not it matched.
    let mut match_required = false;
    let mut remaining = columns;
    for row in (0..rows).rev() {
        while remaining > 0 {
            let column = remaining - 1;
            remaining -= 1;
            let index = row * columns + column;
            if best_end[index] != f64::NEG_INFINITY
                && (match_required || best_end[index] == best_match[index])
            {
                match_required = row > 0
                    && column > 0
                    && best_match[index] == best_end[(row - 1) * columns + column - 1] + SCORE_MATCH_CONSECUTIVE;
                if let Some(slot) = positions.get_mut(row) {
                    *slot = column;
                }
                break;
            }
        }
    }

    best_match[(rows - 1) * columns + (columns - 1)]
}

/// Convert an fzy score to upstream's integer score (`fuzzy.c:122-129`).
fn scale_score(fzy: f64) -> i32 {
    if fzy == f64::NEG_INFINITY {
        return SCORE_NONE;
    }
    if fzy == f64::INFINITY {
        return i32::MAX;
    }
    let scaled = if fzy < 0.0 { (fzy * SCORE_SCALE - 0.5).ceil() } else { (fzy * SCORE_SCALE + 0.5).floor() };
    if scaled >= f64::from(i32::MAX) {
        i32::MAX
    } else if scaled <= f64::from(i32::MIN) {
        i32::MIN
    } else {
        scaled as i32
    }
}

/// `fuzzy_match` (`fuzzy.c:75-158`): match every blank-separated word of
/// `pattern` in `haystack`, accumulating scores and positions. `matchseq`
/// treats the whole pattern as one word.
///
/// Returns `None` when nothing matched, which is upstream's
/// `numMatches == 0`.
pub(crate) fn fuzzy_match(haystack: &[char], pattern: &[char], matchseq: bool) -> Option<FuzzyMatch> {
    let mut positions = vec![0usize; MATCH_MAX_LEN];
    let mut matched = 0usize;
    let mut total: i32 = 0;
    let mut cursor = 0usize;

    loop {
        let word: &[char];
        let complete;
        if matchseq {
            word = pattern;
            complete = true;
        } else {
            while cursor < pattern.len() && is_white(pattern[cursor]) {
                cursor += 1;
            }
            if cursor >= pattern.len() {
                break;
            }
            let start = cursor;
            while cursor < pattern.len() && !is_white(pattern[cursor]) {
                cursor += 1;
            }
            word = &pattern[start..cursor];
            complete = cursor >= pattern.len();
        }

        let word_len = word.len().min(MATCH_MAX_LEN);
        if matched > MATCH_MAX_LEN - word_len {
            return None;
        }

        let mut score = SCORE_NONE;
        if has_match(word, haystack) {
            score = scale_score(match_positions(word, haystack, &mut positions[matched..]));
        }
        if score == SCORE_NONE {
            return None;
        }

        if score > 0 && total > i32::MAX - score {
            total = i32::MAX;
        } else if score < 0 && total < i32::MIN + 1 - score {
            total = i32::MIN + 1;
        } else {
            total += score;
        }

        matched += word_len;
        if complete || matched >= MATCH_MAX_LEN {
            break;
        }
        cursor += 1;
    }

    if matched == 0 {
        return None;
    }
    positions.truncate(matched);
    Some(FuzzyMatch { score: total, positions })
}
