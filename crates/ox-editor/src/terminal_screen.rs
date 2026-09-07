//! Terminal screen model behind a terminal channel buffer.
//!
//! Upstream hands PTY bytes to libvterm and keeps the parsed screen inside a
//! `Terminal` struct, then projects that screen back out as buffer lines. This
//! module ports both halves of that pair, because libvterm is not vendored:
//!
//! * [`TerminalScreen::write`] is `terminal_receive`
//!   (`.references/neovim/src/nvim/terminal.c:1382-1420`): bytes go in, screen
//!   cells are updated, and the touched rows are recorded as damage. The parse
//!   state lives on the struct rather than on the stack so a control sequence
//!   split across two PTY reads still parses, exactly as `vterm_input_write`
//!   (`terminal.c:1401`) keeps its state between calls.
//! * [`TerminalScreen::render_row`] is `fetch_row` (`terminal.c:2326-2346`):
//!   cells become one buffer line, and the never-written tail is dropped
//!   because upstream only advances `line_len` for cells with a glyph
//!   (`terminal.c:2335-2341`).
//! * [`TerminalScreen::line_of_row`] is `row_to_linenr`
//!   (`terminal.c:2691-2694`): the buffer holds scrollback first and the
//!   visible screen after it, so screen row `r` is buffer line
//!   `r + scrollback + 1`.
//! * Scrollback growth is `term_sb_push` (`terminal.c:1699-1750`), including
//!   the eviction of the oldest row once the budget is full
//!   (`terminal.c:1711-1722`).
//! * The alternate screen is the `VTERM_PROP_ALTSCREEN` arm of
//!   `term_settermprop` (`terminal.c:1606-1608`). Upstream keeps scrollback
//!   untouched while it is up: `term_sb_clear` returns early on
//!   `in_altscreen` (`terminal.c:1798`), and libvterm only pushes scrollback
//!   for the primary buffer.
//! * Cursor reporting is `term_movecursor` (`terminal.c:1572-1577`) plus
//!   `terminal_check_cursor` (`terminal.c:994-998`), which places the window
//!   cursor on `row_to_linenr(cursor.row)`.

use std::collections::VecDeque;

use unicode_width::UnicodeWidthChar;

/// Default scrollback budget, upstream `'scrollback'` default.
const DEFAULT_SCROLLBACK: usize = 10_000;

/// Power-on tab stop interval of a VT100.
const TAB_WIDTH: usize = 8;

/// Upper bound on parameters kept for one control sequence. libvterm caps this
/// at `CSI_ARGS_MAX`; the exact bound only has to stop a hostile child from
/// growing the accumulator without limit.
const MAX_CSI_PARAMS: usize = 32;

/// One screen colour, in the three forms an SGR sequence can name.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TermColor {
    /// The palette default, selected by `SGR 39` and `SGR 49`.
    #[default]
    Default,
    /// One of the 256 indexed palette entries.
    Indexed(u8),
    /// A direct 24-bit colour from `SGR 38;2` or `SGR 48;2`.
    Rgb(u8, u8, u8),
}

/// Boolean SGR attributes carried by one cell.
///
/// Packed into a single integer because one instance is stored per cell and a
/// default 80x24 screen holds 1920 of them.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CellFlags(u16);

impl CellFlags {
    /// `SGR 1`.
    pub const BOLD: Self = Self(1 << 0);
    /// `SGR 3`.
    pub const ITALIC: Self = Self(1 << 1);
    /// `SGR 4`, upstream `HL_UNDERLINE` (`terminal.c:1434`).
    pub const UNDERLINE: Self = Self(1 << 2);
    /// `SGR 4:3`, upstream `HL_UNDERCURL` (`terminal.c:1438`).
    pub const UNDERCURL: Self = Self(1 << 3);
    /// `SGR 4:2` and `SGR 21`, upstream `HL_UNDERDOUBLE` (`terminal.c:1436`).
    pub const UNDERDOUBLE: Self = Self(1 << 4);
    /// `SGR 7`.
    pub const REVERSE: Self = Self(1 << 5);
    /// `SGR 9`.
    pub const STRIKETHROUGH: Self = Self(1 << 6);
    /// `SGR 5` and `SGR 6`.
    pub const BLINK: Self = Self(1 << 7);
    /// `SGR 2`.
    pub const FAINT: Self = Self(1 << 8);
    /// Lead cell of a double-width glyph; the following cell is its tail.
    pub const WIDE: Self = Self(1 << 9);

    /// Every underline style, cleared together by `SGR 24`.
    const UNDERLINES: Self = Self(Self::UNDERLINE.0 | Self::UNDERCURL.0 | Self::UNDERDOUBLE.0);

    /// No attributes set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Whether every flag of `other` is present.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// The flags of both operands.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// These flags with every flag of `other` removed.
    #[must_use]
    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// Whether no flag is set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The raw bit pattern, for stable cache keys.
    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }
}

/// The pen a cell was written with: what a highlight span has to reproduce.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CellAttrs {
    /// Foreground colour.
    pub foreground: TermColor,
    /// Background colour.
    pub background: TermColor,
    /// Boolean attributes.
    pub flags: CellFlags,
}

impl CellAttrs {
    /// Whether this pen is the terminal default and so needs no highlight.
    ///
    /// [`CellFlags::WIDE`] is a layout marker rather than an SGR attribute, so
    /// it does not make a pen non-default.
    #[must_use]
    pub const fn is_default(self) -> bool {
        matches!(self.foreground, TermColor::Default)
            && matches!(self.background, TermColor::Default)
            && self.flags.without(CellFlags::WIDE).is_empty()
    }
}

/// Terminal cursor position, in screen rows and columns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TerminalCursor {
    /// Zero-based screen row.
    pub row: usize,
    /// Zero-based screen column.
    pub col: usize,
    /// Whether the child asked for a visible cursor (`DECTCEM`, upstream
    /// `VTERM_PROP_CURSORVISIBLE`, `terminal.c:1610-1613`).
    pub visible: bool,
}

impl Default for TerminalCursor {
    fn default() -> Self {
        Self {
            row: 0,
            col: 0,
            visible: true,
        }
    }
}

/// Cell geometry of a terminal screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ScreenSize {
    /// Visible rows.
    pub rows: usize,
    /// Visible columns.
    pub cols: usize,
}

impl ScreenSize {
    /// Geometry clamped to at least one cell in each dimension.
    #[must_use]
    pub const fn new(rows: usize, cols: usize) -> Self {
        Self {
            rows: if rows == 0 { 1 } else { rows },
            cols: if cols == 0 { 1 } else { cols },
        }
    }
}

/// A run of bytes in a rendered line that shares one pen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AttrSpan {
    /// Inclusive byte offset into [`RenderedRow::text`].
    pub start: usize,
    /// Exclusive byte offset into [`RenderedRow::text`].
    pub end: usize,
    /// Pen shared by every cell of the run.
    pub attrs: CellAttrs,
}

/// One screen or scrollback row projected into buffer-line form.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct RenderedRow {
    /// Line bytes with the never-written tail dropped, upstream `fetch_row`
    /// (`terminal.c:2326-2346`).
    pub text: Vec<u8>,
    /// Byte runs of the line that carry a non-default pen, in ascending order.
    pub spans: Vec<AttrSpan>,
}

/// What changed since the previous refresh.
///
/// Mirrors upstream's `invalid_start`/`invalid_end` pair together with
/// `sb_pending` and `sb_deleted` (`terminal.c:2370-2375`, `:1718`, `:1740`),
/// which is what lets `refresh_screen` and `refresh_scrollback` touch only the
/// buffer lines that actually moved.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Damage {
    /// Screen rows whose cells changed.
    pub rows: std::ops::Range<usize>,
    /// Rows pushed into scrollback since the previous refresh.
    pub scrollback_pushed: usize,
    /// Scrollback rows evicted from the oldest end since the previous refresh.
    pub scrollback_deleted: usize,
    /// Whether the projection must be rebuilt wholesale because the line-number
    /// mapping moved under the consumer (a resize, or `CSI 3 J`).
    pub resync: bool,
}

/// One screen cell.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Cell {
    /// Glyph in the cell, or `None` both for a cell never written and for the
    /// tail of a double-width glyph. Upstream distinguishes those from a
    /// written space through `VTermScreenCell.schar == 0`
    /// (`terminal.c:2335`), which is what makes `fetch_row` drop the padding
    /// after the last real glyph.
    ch: Option<char>,
    /// Pen the cell was written with.
    attrs: CellAttrs,
}

impl Cell {
    /// A cell that was never written.
    const EMPTY: Self = Self {
        ch: None,
        attrs: CellAttrs {
            foreground: TermColor::Default,
            background: TermColor::Default,
            flags: CellFlags::empty(),
        },
    };

    /// A cell erased with `attrs`: no glyph, but the erasing pen's background.
    const fn erased(attrs: CellAttrs) -> Self {
        Self {
            ch: None,
            attrs: CellAttrs {
                foreground: TermColor::Default,
                background: attrs.background,
                flags: CellFlags::empty(),
            },
        }
    }
}

/// Parser position, carried across [`TerminalScreen::write`] calls.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum ParseState {
    /// Outside any escape sequence.
    #[default]
    Ground,
    /// Mid multi-byte UTF-8 scalar.
    Utf8,
    /// Saw `ESC`.
    Esc,
    /// Inside `CSI`, collecting parameters.
    Csi,
    /// Inside an OSC, DCS, PM or APC string, swallowed to its terminator.
    StringSeq,
    /// One designator byte to swallow, as in `ESC ( B`.
    Discard,
}

/// Partially decoded UTF-8 scalar.
#[derive(Clone, Copy, Debug, Default)]
struct Utf8Accumulator {
    bytes: [u8; 4],
    len: usize,
    need: usize,
}

/// Parameters of the control sequence being collected.
#[derive(Clone, Debug, Default)]
struct CsiAccumulator {
    /// Parameter values; `None` is an omitted parameter taking its default.
    params: Vec<Option<u32>>,
    /// Whether the same-index parameter was introduced by `:` rather than `;`,
    /// making it a sub-parameter of the one before. `SGR 4:3` (undercurl) and
    /// `SGR 38:2:r:g:b` both need that distinction, and collapsing `:` onto
    /// `;` would read `4:3` as underline plus blink.
    subparam: Vec<bool>,
    /// Private-marker byte (`<`, `=`, `>`, `?`).
    private: Option<u8>,
    /// Intermediate byte (`0x20..=0x2f`).
    intermediate: Option<u8>,
}

impl CsiAccumulator {
    fn reset(&mut self) {
        self.params.clear();
        self.subparam.clear();
        self.private = None;
        self.intermediate = None;
    }

    /// Parameter `index`, or `default` when omitted or zero.
    ///
    /// Zero selects the default for every cursor-movement and scroll sequence,
    /// which is why `CSI 0 A` moves one row just like `CSI A`.
    fn count(&self, index: usize, default: usize) -> usize {
        match self.params.get(index).copied().flatten() {
            None | Some(0) => default,
            Some(value) => usize::try_from(value).unwrap_or(default),
        }
    }

    /// Parameter `index` as a raw selector, defaulting to zero.
    fn selector(&self, index: usize) -> u32 {
        self.params.get(index).copied().flatten().unwrap_or(0)
    }

    /// Whether parameter `index` is a sub-parameter of the one before it.
    fn is_subparam(&self, index: usize) -> bool {
        self.subparam.get(index).copied().unwrap_or(false)
    }
}

/// Parsed VT screen behind one terminal buffer.
#[derive(Clone, Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the parser mirrors the vterm state machine one-to-one"
)]
pub struct TerminalScreen {
    size: ScreenSize,
    /// Active screen cells, row-major, `rows * cols` long.
    cells: Vec<Cell>,
    parked: Option<Vec<Cell>>,
    in_altscreen: bool,
    scrollback: VecDeque<Vec<Cell>>,
    scrollback_limit: usize,
    cursor: TerminalCursor,
    saved_cursor: TerminalCursor,
    pen: CellAttrs,
    state: ParseState,
    utf8: Utf8Accumulator,
    csi: CsiAccumulator,
    /// Whether the string sequence being swallowed just saw an `ESC`, so a
    /// following `\` closes it as `ST`.
    string_esc: bool,
    /// Inclusive first row of the DECSTBM scroll region.
    scroll_top: usize,
    /// Inclusive last row of the DECSTBM scroll region.
    scroll_bottom: usize,
    /// Deferred wrap: the cursor sits on the last column and the next glyph
    /// wraps before it is written.
    pending_wrap: bool,
    autowrap: bool,
    damage_start: usize,
    damage_end: usize,
    scrollback_pushed: usize,
    scrollback_deleted: usize,
    resync: bool,
    /// Bytes owed to the child, from `DSR` and `DA` queries.
    replies: Vec<u8>,
}

impl TerminalScreen {
    /// A screen of `size`, fully damaged so the first refresh writes every row.
    ///
    /// `terminal_open` refreshes the whole screen into the buffer before the
    /// child produces anything (`terminal.c:601`), which is what pre-fills a
    /// terminal buffer with its viewport rows.
    #[must_use]
    pub fn new(size: ScreenSize) -> Self {
        let size = ScreenSize::new(size.rows, size.cols);
        Self {
            cells: vec![Cell::EMPTY; size.rows.saturating_mul(size.cols)],
            parked: None,
            in_altscreen: false,
            scrollback: VecDeque::new(),
            scrollback_limit: DEFAULT_SCROLLBACK,
            cursor: TerminalCursor::default(),
            saved_cursor: TerminalCursor::default(),
            pen: CellAttrs::default(),
            state: ParseState::Ground,
            utf8: Utf8Accumulator::default(),
            csi: CsiAccumulator::default(),
            string_esc: false,
            scroll_top: 0,
            scroll_bottom: size.rows.saturating_sub(1),
            pending_wrap: false,
            autowrap: true,
            damage_start: 0,
            damage_end: size.rows,
            scrollback_pushed: 0,
            scrollback_deleted: 0,
            resync: false,
            replies: Vec::new(),
            size,
        }
    }

    /// Visible row count.
    #[must_use]
    pub const fn rows(&self) -> usize {
        self.size.rows
    }

    /// Visible column count.
    #[must_use]
    pub const fn cols(&self) -> usize {
        self.size.cols
    }

    /// Stored scrollback rows.
    #[must_use]
    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    /// Current cursor position and visibility.
    #[must_use]
    pub const fn cursor(&self) -> TerminalCursor {
        self.cursor
    }

    /// Whether the alternate screen is active.
    #[must_use]
    pub const fn in_altscreen(&self) -> bool {
        self.in_altscreen
    }

    /// Buffer line holding screen row `row`, one-based.
    ///
    /// `row_to_linenr` (`terminal.c:2691-2694`).
    #[must_use]
    pub fn line_of_row(&self, row: usize) -> usize {
        row.saturating_add(self.scrollback.len()).saturating_add(1)
    }

    /// Buffer lines the projection occupies: scrollback then the screen.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.scrollback.len().saturating_add(self.size.rows)
    }

    /// Replace the `'scrollback'` budget, trimming any excess immediately.
    ///
    /// `adjust_scrollback` (`terminal.c:2529-2560`).
    pub fn set_scrollback_limit(&mut self, limit: usize) {
        self.scrollback_limit = limit;
        while self.scrollback.len() > limit {
            if self.scrollback.pop_front().is_none() {
                break;
            }
            self.resync = true;
        }
    }

    /// Take the bytes owed to the child in answer to its queries.
    ///
    /// Upstream routes these through `terminal_send`
    /// (`term_output_callback`, `terminal.c:461-464`).
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    /// Byte offset of the cursor column within the projected line.
    ///
    /// Buffer cursors are byte columns while the emulator tracks cell columns;
    /// multi-byte glyphs make the two differ, and `w_cursor.col` must land
    /// after every glyph left of the cursor (`terminal_check_cursor`,
    /// `terminal.c:994-998`).
    #[must_use]
    pub fn cursor_byte_column(&self) -> usize {
        let start = self.cursor.row.saturating_mul(self.size.cols);
        let end = start.saturating_add(self.cursor.col.min(self.size.cols));
        let bytes: usize = self
            .cells
            .get(start..end)
            .unwrap_or(&[])
            .iter()
            .map(|cell| cell.ch.map_or(1, char::len_utf8))
            .sum();
        bytes.min(self.render_row(self.cursor.row).text.len())
    }
    /// Resize the screen, keeping content anchored at the top.
    ///
    /// `terminal_check_size` (`terminal.c:772`) forwards the window geometry to
    /// `vterm_set_size`. Reflow is left to the child: it is told the new size
    /// and repaints, which is what upstream relies on for full-screen programs.
    pub fn resize(&mut self, size: ScreenSize) {
        let size = ScreenSize::new(size.rows, size.cols);
        if size == self.size {
            return;
        }
        let mut cells = vec![Cell::EMPTY; size.rows.saturating_mul(size.cols)];
        let rows = size.rows.min(self.size.rows);
        let cols = size.cols.min(self.size.cols);
        for row in 0..rows {
            for col in 0..cols {
                let Some(&cell) = self.cells.get(row.saturating_mul(self.size.cols) + col) else {
                    continue;
                };
                if let Some(slot) = cells.get_mut(row.saturating_mul(size.cols) + col) {
                    *slot = cell;
                }
            }
        }
        self.cells = cells;
        self.parked = None;
        self.size = size;
        self.scroll_top = 0;
        self.scroll_bottom = size.rows.saturating_sub(1);
        self.pending_wrap = false;
        self.clamp_cursor();
        self.resync = true;
        self.touch_all();
    }

    /// Feed PTY bytes into the screen.
    ///
    /// `terminal_receive` (`terminal.c:1382-1420`). The parse state is struct
    /// state, so a sequence split between two reads resumes where it stopped.
    pub fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            match self.state {
                ParseState::Ground => self.ground(byte),
                ParseState::Utf8 => self.utf8_continue(byte),
                ParseState::Esc => self.escape(byte),
                ParseState::Csi => self.csi(byte),
                ParseState::StringSeq => self.string_seq(byte),
                ParseState::Discard => self.state = ParseState::Ground,
            }
        }
    }

    /// Take the damage accumulated since the previous call.
    pub fn take_damage(&mut self) -> Option<Damage> {
        let scrollback_pushed = std::mem::take(&mut self.scrollback_pushed);
        let scrollback_deleted = std::mem::take(&mut self.scrollback_deleted);
        let resync = std::mem::take(&mut self.resync);
        let start = self.damage_start;
        let end = self.damage_end;
        self.damage_start = usize::MAX;
        self.damage_end = 0;
        let rows = if start < end {
            start..end.min(self.size.rows)
        } else {
            0..0
        };
        if rows.is_empty() && scrollback_pushed == 0 && scrollback_deleted == 0 && !resync {
            return None;
        }
        Some(Damage {
            rows,
            scrollback_pushed,
            scrollback_deleted,
            resync,
        })
    }

    /// Project screen row `row` into buffer-line form.
    ///
    /// `fetch_row` (`terminal.c:2326-2346`).
    #[must_use]
    pub fn render_row(&self, row: usize) -> RenderedRow {
        let start = row.saturating_mul(self.size.cols);
        let end = start.saturating_add(self.size.cols);
        Self::render(self.cells.get(start..end).unwrap_or(&[]))
    }

    /// Project scrollback row `index`, counting the oldest row as zero.
    ///
    /// `fetch_row` reaching a negative row through `fetch_cell`
    /// (`terminal.c:2350-2361`).
    #[must_use]
    pub fn render_scrollback(&self, index: usize) -> RenderedRow {
        self.scrollback
            .get(index)
            .map_or_else(RenderedRow::default, |row| Self::render(row))
    }

    /// Project one row of cells, dropping the never-written tail.
    fn render(cells: &[Cell]) -> RenderedRow {
        let mut text = Vec::with_capacity(cells.len());
        let mut spans: Vec<AttrSpan> = Vec::new();
        // Upstream only advances `line_len` for cells that carry a glyph, so a
        // run of untouched cells after the last glyph never reaches the buffer
        // line (`terminal.c:2335-2341`).
        let mut written = 0;
        let mut col = 0;
        while let Some(&cell) = cells.get(col) {
            let start = text.len();
            match cell.ch {
                Some(ch) => {
                    let mut encoded = [0u8; 4];
                    text.extend_from_slice(ch.encode_utf8(&mut encoded).as_bytes());
                    written = text.len();
                }
                None => text.push(b' '),
            }
            if !cell.attrs.is_default() {
                let end = text.len();
                match spans.last_mut() {
                    Some(last) if last.end == start && last.attrs == cell.attrs => last.end = end,
                    _ => spans.push(AttrSpan {
                        start,
                        end,
                        attrs: cell.attrs,
                    }),
                }
            }
            // A double-width glyph owns the following cell; upstream skips it
            // with `col += cell.width` (`terminal.c:2341`).
            col += if cell.attrs.flags.contains(CellFlags::WIDE) {
                2
            } else {
                1
            };
        }
        text.truncate(written);
        spans.retain_mut(|span| {
            span.end = span.end.min(written);
            span.start < span.end
        });
        RenderedRow { text, spans }
    }

    // -- damage bookkeeping ------------------------------------------------

    fn touch(&mut self, start: usize, end: usize) {
        self.damage_start = self.damage_start.min(start);
        self.damage_end = self.damage_end.max(end.min(self.size.rows));
    }

    fn touch_all(&mut self) {
        self.damage_start = 0;
        self.damage_end = self.size.rows;
    }

    fn touch_cursor_row(&mut self) {
        let row = self.cursor.row;
        self.touch(row, row.saturating_add(1));
    }

    // -- cell access -------------------------------------------------------

    fn set_cell(&mut self, row: usize, col: usize, cell: Cell) {
        if col >= self.size.cols || row >= self.size.rows {
            return;
        }
        if let Some(slot) = self.cells.get_mut(row.saturating_mul(self.size.cols) + col) {
            *slot = cell;
        }
    }

    fn row_range(&self, row: usize) -> std::ops::Range<usize> {
        let start = row.saturating_mul(self.size.cols);
        start..start.saturating_add(self.size.cols)
    }

    fn clamp_cursor(&mut self) {
        self.cursor.row = self.cursor.row.min(self.size.rows.saturating_sub(1));
        self.cursor.col = self.cursor.col.min(self.size.cols.saturating_sub(1));
    }

    // -- control functions -------------------------------------------------

    fn carriage_return(&mut self) {
        self.cursor.col = 0;
        self.pending_wrap = false;
    }

    fn backspace(&mut self) {
        self.cursor.col = self.cursor.col.saturating_sub(1);
        self.pending_wrap = false;
    }

    fn tab(&mut self) {
        let next = self
            .cursor
            .col
            .saturating_div(TAB_WIDTH)
            .saturating_add(1)
            .saturating_mul(TAB_WIDTH);
        self.cursor.col = next.min(self.size.cols.saturating_sub(1));
        self.pending_wrap = false;
    }

    fn line_feed(&mut self) {
        self.pending_wrap = false;
        self.advance_row();
    }

    /// Move down one row, scrolling when already on the region's last row.
    fn advance_row(&mut self) {
        if self.cursor.row == self.scroll_bottom {
            self.scroll_up(1);
        } else if self.cursor.row.saturating_add(1) < self.size.rows {
            self.cursor.row += 1;
        }
    }

    /// `RI`: move up one row, scrolling down at the region's first row.
    fn reverse_index(&mut self) {
        if self.cursor.row == self.scroll_top {
            self.scroll_down(1);
        } else {
            self.cursor.row = self.cursor.row.saturating_sub(1);
        }
        self.pending_wrap = false;
    }

    fn region_height(&self) -> usize {
        self.scroll_bottom
            .saturating_sub(self.scroll_top)
            .saturating_add(1)
    }

    fn scroll_up(&mut self, count: usize) {
        let count = count.min(self.region_height());
        if count == 0 {
            return;
        }
        // Only a full-height region on the primary screen feeds scrollback:
        // upstream's scrollback callbacks belong to the primary buffer, and
        // `term_sb_clear` refuses to touch scrollback while the alternate
        // screen is up (`terminal.c:1798`).
        let full_height =
            self.scroll_top == 0 && self.scroll_bottom.saturating_add(1) == self.size.rows;
        if full_height && !self.in_altscreen {
            for offset in 0..count {
                self.push_scrollback(self.scroll_top.saturating_add(offset));
            }
        }
        let blank = Cell::erased(self.pen);
        for row in self.scroll_top..=self.scroll_bottom {
            let source = row.saturating_add(count);
            if source <= self.scroll_bottom {
                let from = self.row_range(source);
                let to = self.row_range(row);
                self.cells.copy_within(from, to.start);
            } else {
                let range = self.row_range(row);
                if let Some(slice) = self.cells.get_mut(range) {
                    slice.fill(blank);
                }
            }
        }
        self.touch(self.scroll_top, self.scroll_bottom.saturating_add(1));
    }

    fn scroll_down(&mut self, count: usize) {
        let count = count.min(self.region_height());
        if count == 0 {
            return;
        }
        let blank = Cell::erased(self.pen);
        for row in (self.scroll_top..=self.scroll_bottom).rev() {
            match row.checked_sub(count) {
                Some(source) if source >= self.scroll_top => {
                    let from = self.row_range(source);
                    let to = self.row_range(row);
                    self.cells.copy_within(from, to.start);
                }
                _ => {
                    let range = self.row_range(row);
                    if let Some(slice) = self.cells.get_mut(range) {
                        slice.fill(blank);
                    }
                }
            }
        }
        self.touch(self.scroll_top, self.scroll_bottom.saturating_add(1));
    }

    /// `term_sb_push` (`terminal.c:1699-1750`).
    fn push_scrollback(&mut self, row: usize) {
        if self.scrollback_limit == 0 {
            return;
        }
        let range = self.row_range(row);
        let Some(cells) = self.cells.get(range) else {
            return;
        };
        let cells = cells.to_vec();
        if self.scrollback.len() >= self.scrollback_limit {
            // Upstream recycles the oldest row and counts it in `sb_deleted`
            // (`terminal.c:1711-1722`).
            if self.scrollback.pop_front().is_some() {
                self.scrollback_deleted = self.scrollback_deleted.saturating_add(1);
            }
        }
        self.scrollback.push_back(cells);
        self.scrollback_pushed = self.scrollback_pushed.saturating_add(1);
    }

    /// `term_sb_clear` (`terminal.c:1794-1812`).
    fn clear_scrollback(&mut self) {
        if self.in_altscreen || self.scrollback.is_empty() {
            return;
        }
        self.scrollback.clear();
        self.scrollback_pushed = 0;
        self.scrollback_deleted = 0;
        self.resync = true;
        self.touch_all();
    }

    fn put_char(&mut self, ch: char) {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width == 0 {
            // A zero-width mark belongs to the preceding cell. Without
            // multi-glyph cells the base character is what keeps the columns
            // aligned, so drop the mark rather than let it consume a column
            // and shift the rest of the row.
            return;
        }
        if self.pending_wrap {
            self.pending_wrap = false;
            if self.autowrap {
                self.cursor.col = 0;
                self.advance_row();
            }
        }
        if width == 2 && self.cursor.col.saturating_add(2) > self.size.cols && self.autowrap {
            self.cursor.col = 0;
            self.advance_row();
        }
        let (row, col) = (self.cursor.row, self.cursor.col);
        let mut attrs = self.pen;
        if width == 2 {
            attrs.flags = attrs.flags.union(CellFlags::WIDE);
        }
        self.set_cell(
            row,
            col,
            Cell {
                ch: Some(ch),
                attrs,
            },
        );
        if width == 2 {
            self.set_cell(
                row,
                col.saturating_add(1),
                Cell {
                    ch: None,
                    attrs: self.pen,
                },
            );
        }
        let next = col.saturating_add(width);
        if next >= self.size.cols {
            self.cursor.col = self.size.cols.saturating_sub(1);
            self.pending_wrap = self.autowrap;
        } else {
            self.cursor.col = next;
        }
        self.touch(row, row.saturating_add(1));
    }

    /// `RIS`: full reset.
    fn reset(&mut self) {
        let blank = Cell::EMPTY;
        self.cells.fill(blank);
        self.parked = None;
        self.in_altscreen = false;
        self.cursor = TerminalCursor::default();
        self.saved_cursor = TerminalCursor::default();
        self.pen = CellAttrs::default();
        self.scroll_top = 0;
        self.scroll_bottom = self.size.rows.saturating_sub(1);
        self.pending_wrap = false;
        self.autowrap = true;
        self.touch_all();
    }

    // -- parser ------------------------------------------------------------

    fn ground(&mut self, byte: u8) {
        match byte {
            // NUL, the legacy envelopes, BEL (`term_bell`, `terminal.c:1681`),
            // SO/SI charset selection with no alternate set, the remaining C0
            // controls, and DEL record nothing.
            0x00..=0x07 | 0x0e..=0x1a | 0x1c..=0x1f | 0x7f => {}
            0x08 => self.backspace(),
            0x09 => self.tab(),
            0x0a..=0x0c => self.line_feed(),
            0x0d => self.carriage_return(),
            0x1b => self.state = ParseState::Esc,
            0x20..=0x7e => self.put_char(char::from(byte)),
            0x80..=0xff => self.utf8_begin(byte),
        }
    }

    fn utf8_begin(&mut self, byte: u8) {
        let need = match byte {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            // A continuation byte with no lead, or an overlong or out-of-range
            // lead. libvterm substitutes U+FFFD; match that.
            _ => {
                self.put_char(char::REPLACEMENT_CHARACTER);
                return;
            }
        };
        self.utf8 = Utf8Accumulator {
            bytes: [byte, 0, 0, 0],
            len: 1,
            need,
        };
        self.state = ParseState::Utf8;
    }

    fn utf8_continue(&mut self, byte: u8) {
        if !(0x80..=0xbf).contains(&byte) {
            // Truncated scalar. Emit the replacement and reprocess this byte
            // in ground state so a following ESC is not swallowed.
            self.state = ParseState::Ground;
            self.put_char(char::REPLACEMENT_CHARACTER);
            self.ground(byte);
            return;
        }
        if let Some(slot) = self.utf8.bytes.get_mut(self.utf8.len) {
            *slot = byte;
            self.utf8.len = self.utf8.len.saturating_add(1);
        }
        if self.utf8.len < self.utf8.need {
            return;
        }
        let accumulated = self.utf8;
        self.state = ParseState::Ground;
        match std::str::from_utf8(accumulated.bytes.get(..accumulated.len).unwrap_or(&[])) {
            Ok(text) => {
                for ch in text.chars() {
                    self.put_char(ch);
                }
            }
            Err(_) => self.put_char(char::REPLACEMENT_CHARACTER),
        }
    }

    fn escape(&mut self, byte: u8) {
        self.state = ParseState::Ground;
        match byte {
            b'[' => {
                self.csi.reset();
                self.state = ParseState::Csi;
            }
            // OSC, DCS, PM and APC all run to a string terminator.
            b']' | b'P' | b'^' | b'_' => {
                self.string_esc = false;
                self.state = ParseState::StringSeq;
            }
            // Character-set designators and other two-byte forms.
            b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' | b'#' | b'%' | b' ' => {
                self.state = ParseState::Discard;
            }
            // DECSC and DECRC.
            b'7' => self.saved_cursor = self.cursor,
            b'8' => {
                self.cursor = self.saved_cursor;
                self.clamp_cursor();
                self.pending_wrap = false;
            }
            b'c' => self.reset(),
            // IND, NEL and RI.
            b'D' => self.line_feed(),
            b'E' => {
                self.carriage_return();
                self.advance_row();
            }
            b'M' => self.reverse_index(),
            0x1b => self.state = ParseState::Esc,
            _ => {}
        }
    }

    fn string_seq(&mut self, byte: u8) {
        // OSC strings end at BEL or at ST (`ESC \`).
        if self.string_esc {
            self.string_esc = false;
            match byte {
                b'\\' => self.state = ParseState::Ground,
                0x1b => self.string_esc = true,
                _ => {}
            }
            return;
        }
        match byte {
            0x07 => self.state = ParseState::Ground,
            0x1b => self.string_esc = true,
            _ => {}
        }
    }

    fn csi(&mut self, byte: u8) {
        match byte {
            b'0'..=b'9' => {
                if self.csi.params.is_empty() {
                    self.csi.params.push(None);
                    self.csi.subparam.push(false);
                }
                let digit = u32::from(byte.saturating_sub(b'0'));
                if let Some(slot) = self.csi.params.last_mut() {
                    let value = slot
                        .unwrap_or(0)
                        .saturating_mul(10)
                        .saturating_add(digit)
                        .min(u32::from(u16::MAX));
                    *slot = Some(value);
                }
            }
            b';' | b':' => {
                if self.csi.params.len() < MAX_CSI_PARAMS {
                    self.csi.params.push(None);
                    self.csi.subparam.push(byte == b':');
                }
            }
            0x3c..=0x3f => self.csi.private = Some(byte),
            0x20..=0x2f => self.csi.intermediate = Some(byte),
            0x40..=0x7e => {
                self.state = ParseState::Ground;
                self.dispatch_csi(byte);
            }
            0x1b => self.state = ParseState::Esc,
            // A C0 control inside a control sequence executes immediately and
            // the sequence continues, which is what xterm does.
            _ => self.ground(byte),
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one match arm per CSI final byte; splitting it would hide the dispatch table"
    )]
    fn dispatch_csi(&mut self, final_byte: u8) {
        if self.csi.intermediate.is_some() {
            // Sequences with an intermediate byte are soft-reset, cursor-style
            // and similar requests that leave no mark on the screen model.
            return;
        }
        let last_row = self.size.rows.saturating_sub(1);
        let last_col = self.size.cols.saturating_sub(1);
        match final_byte {
            // ICH
            b'@' => self.insert_cells(self.csi.count(0, 1)),
            // CUU, CUD, CUF, CUB
            b'A' => {
                self.cursor.row = self.cursor.row.saturating_sub(self.csi.count(0, 1));
                self.pending_wrap = false;
            }
            b'B' | b'e' => {
                self.cursor.row = self
                    .cursor
                    .row
                    .saturating_add(self.csi.count(0, 1))
                    .min(last_row);
                self.pending_wrap = false;
            }
            b'C' | b'a' => {
                self.cursor.col = self
                    .cursor
                    .col
                    .saturating_add(self.csi.count(0, 1))
                    .min(last_col);
                self.pending_wrap = false;
            }
            b'D' => {
                self.cursor.col = self.cursor.col.saturating_sub(self.csi.count(0, 1));
                self.pending_wrap = false;
            }
            // CNL and CPL
            b'E' => {
                self.cursor.row = self
                    .cursor
                    .row
                    .saturating_add(self.csi.count(0, 1))
                    .min(last_row);
                self.carriage_return();
            }
            b'F' => {
                self.cursor.row = self.cursor.row.saturating_sub(self.csi.count(0, 1));
                self.carriage_return();
            }
            // CHA and HPA
            b'G' | b'`' => {
                self.cursor.col = self.csi.count(0, 1).saturating_sub(1).min(last_col);
                self.pending_wrap = false;
            }
            // CUP and HVP
            b'H' | b'f' => {
                self.cursor.row = self.csi.count(0, 1).saturating_sub(1).min(last_row);
                self.cursor.col = self.csi.count(1, 1).saturating_sub(1).min(last_col);
                self.pending_wrap = false;
            }
            // CHT and CBT: the repeat count saturates at the tab stops
            // a screen holds, so a hostile parameter cannot spin billions
            // of no-op iterations past the line boundary.
            b'I' => {
                for _ in 0..self.csi.count(0, 1).min(last_col.saturating_add(1)) {
                    self.tab();
                }
            }
            b'Z' => {
                for _ in 0..self.csi.count(0, 1).min(last_col.saturating_add(1)) {
                    let col = self.cursor.col;
                    self.cursor.col = col.saturating_sub(1) / TAB_WIDTH * TAB_WIDTH;
                }
                self.pending_wrap = false;
            }
            // ED
            b'J' => self.erase_in_display(self.csi.selector(0)),
            // EL
            b'K' => self.erase_in_line(self.csi.selector(0)),
            // IL and DL
            b'L' => self.insert_lines(self.csi.count(0, 1)),
            b'M' => self.delete_lines(self.csi.count(0, 1)),
            // DCH
            b'P' => self.delete_cells(self.csi.count(0, 1)),
            // SU and SD
            b'S' => self.scroll_up(self.csi.count(0, 1)),
            b'T' => self.scroll_down(self.csi.count(0, 1)),
            // ECH
            b'X' => self.erase_cells(self.csi.count(0, 1)),
            // VPA and VPR
            b'd' => {
                self.cursor.row = self.csi.count(0, 1).saturating_sub(1).min(last_row);
                self.pending_wrap = false;
            }
            // DA: report a VT100 with an advanced video option.
            b'c' => {
                if self.csi.private.is_none() {
                    self.replies.extend_from_slice(b"\x1b[?1;2c");
                }
            }
            // SM and RM
            b'h' => self.set_mode(true),
            b'l' => self.set_mode(false),
            // SGR
            b'm' => self.sgr(),
            // DSR
            b'n' => self.device_status(),
            // DECSTBM
            b'r' => {
                let top = self.csi.count(0, 1).saturating_sub(1).min(last_row);
                let bottom = self
                    .csi
                    .count(1, self.size.rows)
                    .saturating_sub(1)
                    .min(last_row);
                if top < bottom {
                    self.scroll_top = top;
                    self.scroll_bottom = bottom;
                } else {
                    self.scroll_top = 0;
                    self.scroll_bottom = last_row;
                }
                self.cursor.row = self.scroll_top;
                self.carriage_return();
            }
            // ANSI.SYS cursor save and restore.
            b's' => self.saved_cursor = self.cursor,
            b'u' => {
                self.cursor = self.saved_cursor;
                self.clamp_cursor();
                self.pending_wrap = false;
            }
            _ => {}
        }
    }

    /// `CSI ? Pm h` and `CSI ? Pm l`, plus the ANSI modes that matter here.
    fn set_mode(&mut self, enable: bool) {
        if self.csi.private != Some(b'?') {
            return;
        }
        for index in 0..self.csi.params.len().max(1) {
            match self.csi.selector(index) {
                // DECAWM
                7 => self.autowrap = enable,
                // DECTCEM, upstream `VTERM_PROP_CURSORVISIBLE`
                // (`terminal.c:1610-1613`).
                25 => {
                    self.cursor.visible = enable;
                    self.touch_cursor_row();
                }
                // The alternate screen, upstream `VTERM_PROP_ALTSCREEN`
                // (`terminal.c:1606-1608`). `47` and `1047` switch buffers
                // only; `1049` also saves and restores the cursor.
                47 | 1047 => self.set_altscreen(enable, false),
                1049 => self.set_altscreen(enable, true),
                _ => {}
            }
        }
    }

    /// Enter or leave the alternate screen.
    ///
    /// The primary cells are parked rather than overwritten, so leaving
    /// restores exactly what the primary screen held; scrollback is untouched
    /// throughout, which is what `altscreen_spec` asserts.
    fn set_altscreen(&mut self, enable: bool, save_cursor: bool) {
        if enable == self.in_altscreen {
            return;
        }
        if enable {
            if save_cursor {
                self.saved_cursor = self.cursor;
            }
            let fresh = vec![Cell::EMPTY; self.size.rows.saturating_mul(self.size.cols)];
            self.parked = Some(std::mem::replace(&mut self.cells, fresh));
        } else {
            if let Some(primary) = self.parked.take() {
                self.cells = primary;
            }
            if save_cursor {
                self.cursor = self.saved_cursor;
            }
        }
        self.in_altscreen = enable;
        self.pending_wrap = false;
        self.clamp_cursor();
        self.touch_all();
    }

    /// `CSI Ps n`.
    fn device_status(&mut self) {
        match self.csi.selector(0) {
            5 => self.replies.extend_from_slice(b"\x1b[0n"),
            6 => {
                let row = self.cursor.row.saturating_add(1);
                let col = self.cursor.col.saturating_add(1);
                self.replies
                    .extend_from_slice(format!("\x1b[{row};{col}R").as_bytes());
            }
            _ => {}
        }
    }

    fn erase_in_line(&mut self, selector: u32) {
        let (start, end) = match selector {
            1 => (0, self.cursor.col.saturating_add(1)),
            2 => (0, self.size.cols),
            _ => (self.cursor.col, self.size.cols),
        };
        let blank = Cell::erased(self.pen);
        let row = self.cursor.row;
        let base = self.row_range(row).start;
        for col in start..end.min(self.size.cols) {
            if let Some(slot) = self.cells.get_mut(base.saturating_add(col)) {
                *slot = blank;
            }
        }
        self.pending_wrap = false;
        self.touch(row, row.saturating_add(1));
    }

    fn erase_in_display(&mut self, selector: u32) {
        // `CSI 3 J` drops the scrollback rather than the screen.
        if selector == 3 {
            self.clear_scrollback();
            return;
        }
        let blank = Cell::erased(self.pen);
        let (first, last) = match selector {
            1 => (0, self.cursor.row),
            2 => (0, self.size.rows.saturating_sub(1)),
            _ => (self.cursor.row, self.size.rows.saturating_sub(1)),
        };
        for row in first..=last.min(self.size.rows.saturating_sub(1)) {
            let partial = row == self.cursor.row && (selector == 0 || selector == 1);
            let range = if partial && selector == 0 {
                self.cursor.col..self.size.cols
            } else if partial {
                0..self.cursor.col.saturating_add(1).min(self.size.cols)
            } else {
                0..self.size.cols
            };
            let base = self.row_range(row).start;
            for col in range {
                if let Some(slot) = self.cells.get_mut(base.saturating_add(col)) {
                    *slot = blank;
                }
            }
        }
        self.pending_wrap = false;
        self.touch(first, last.saturating_add(1));
    }

    fn erase_cells(&mut self, count: usize) {
        let blank = Cell::erased(self.pen);
        let row = self.cursor.row;
        let base = self.row_range(row).start;
        let end = self.cursor.col.saturating_add(count).min(self.size.cols);
        for col in self.cursor.col..end {
            if let Some(slot) = self.cells.get_mut(base.saturating_add(col)) {
                *slot = blank;
            }
        }
        self.touch(row, row.saturating_add(1));
    }

    fn insert_cells(&mut self, count: usize) {
        let row = self.cursor.row;
        let base = self.row_range(row).start;
        let blank = Cell::erased(self.pen);
        let col = self.cursor.col;
        let count = count.min(self.size.cols.saturating_sub(col));
        for target in (col..self.size.cols).rev() {
            let source = target.checked_sub(count).filter(|source| *source >= col);
            let cell = source
                .and_then(|source| self.cells.get(base.saturating_add(source)).copied())
                .unwrap_or(blank);
            if let Some(slot) = self.cells.get_mut(base.saturating_add(target)) {
                *slot = cell;
            }
        }
        self.touch(row, row.saturating_add(1));
    }

    fn delete_cells(&mut self, count: usize) {
        let row = self.cursor.row;
        let base = self.row_range(row).start;
        let blank = Cell::erased(self.pen);
        let col = self.cursor.col;
        for target in col..self.size.cols {
            let cell = target
                .checked_add(count)
                .filter(|source| *source < self.size.cols)
                .and_then(|source| self.cells.get(base.saturating_add(source)).copied())
                .unwrap_or(blank);
            if let Some(slot) = self.cells.get_mut(base.saturating_add(target)) {
                *slot = cell;
            }
        }
        self.touch(row, row.saturating_add(1));
    }

    /// `IL`: open `count` blank rows at the cursor, within the scroll region.
    fn insert_lines(&mut self, count: usize) {
        if self.cursor.row < self.scroll_top || self.cursor.row > self.scroll_bottom {
            return;
        }
        let saved_top = self.scroll_top;
        self.scroll_top = self.cursor.row;
        self.scroll_down(count);
        self.scroll_top = saved_top;
        self.carriage_return();
    }

    /// `DL`: remove `count` rows at the cursor, within the scroll region.
    fn delete_lines(&mut self, count: usize) {
        if self.cursor.row < self.scroll_top || self.cursor.row > self.scroll_bottom {
            return;
        }
        let saved_top = self.scroll_top;
        self.scroll_top = self.cursor.row;
        self.scroll_up(count);
        self.scroll_top = saved_top;
        self.carriage_return();
    }

    fn sgr(&mut self) {
        if self.csi.params.is_empty() {
            self.pen = CellAttrs::default();
            return;
        }
        let mut index = 0;
        while index < self.csi.params.len() {
            let code = self.csi.selector(index);
            match code {
                0 => self.pen = CellAttrs::default(),
                1 => self.pen.flags = self.pen.flags.union(CellFlags::BOLD),
                2 => self.pen.flags = self.pen.flags.union(CellFlags::FAINT),
                3 => self.pen.flags = self.pen.flags.union(CellFlags::ITALIC),
                4 => {
                    // `SGR 4:3` and `SGR 4:2` name the underline style through
                    // a sub-parameter, matching libvterm's
                    // `VTERM_UNDERLINE_*` values (`terminal.c:1428-1441`).
                    let style = if self.csi.is_subparam(index.saturating_add(1)) {
                        let style = self.csi.selector(index.saturating_add(1));
                        index += 1;
                        style
                    } else {
                        1
                    };
                    self.pen.flags = self.pen.flags.without(CellFlags::UNDERLINES);
                    match style {
                        0 => {}
                        2 => self.pen.flags = self.pen.flags.union(CellFlags::UNDERDOUBLE),
                        3 => self.pen.flags = self.pen.flags.union(CellFlags::UNDERCURL),
                        _ => self.pen.flags = self.pen.flags.union(CellFlags::UNDERLINE),
                    }
                }
                5 | 6 => self.pen.flags = self.pen.flags.union(CellFlags::BLINK),
                7 => self.pen.flags = self.pen.flags.union(CellFlags::REVERSE),
                9 => self.pen.flags = self.pen.flags.union(CellFlags::STRIKETHROUGH),
                21 => {
                    self.pen.flags = self
                        .pen
                        .flags
                        .without(CellFlags::UNDERLINES)
                        .union(CellFlags::UNDERDOUBLE);
                }
                22 => {
                    self.pen.flags = self
                        .pen
                        .flags
                        .without(CellFlags::BOLD.union(CellFlags::FAINT));
                }
                23 => self.pen.flags = self.pen.flags.without(CellFlags::ITALIC),
                24 => self.pen.flags = self.pen.flags.without(CellFlags::UNDERLINES),
                25 => self.pen.flags = self.pen.flags.without(CellFlags::BLINK),
                27 => self.pen.flags = self.pen.flags.without(CellFlags::REVERSE),
                29 => self.pen.flags = self.pen.flags.without(CellFlags::STRIKETHROUGH),
                30..=37 => self.pen.foreground = indexed(code.wrapping_sub(30)),
                38 => index = self.extended_color(index, true),
                39 => self.pen.foreground = TermColor::Default,
                40..=47 => self.pen.background = indexed(code.wrapping_sub(40)),
                48 => index = self.extended_color(index, false),
                49 => self.pen.background = TermColor::Default,
                90..=97 => self.pen.foreground = indexed(code.wrapping_sub(90).saturating_add(8)),
                100..=107 => {
                    self.pen.background = indexed(code.wrapping_sub(100).saturating_add(8));
                }
                _ => {}
            }
            index = index.saturating_add(1);
        }
    }

    /// Read a `38`/`48` colour specification, returning the last index used.
    ///
    /// Both the `;`-separated and `:`-separated spellings appear in the wild,
    /// and the parser keeps them distinguishable, so accept either.
    fn extended_color(&mut self, index: usize, foreground: bool) -> usize {
        let kind = self.csi.selector(index.saturating_add(1));
        let (color, last) = match kind {
            2 => {
                let red = self.csi.selector(index.saturating_add(2));
                let green = self.csi.selector(index.saturating_add(3));
                let blue = self.csi.selector(index.saturating_add(4));
                (
                    TermColor::Rgb(clamp_u8(red), clamp_u8(green), clamp_u8(blue)),
                    index.saturating_add(4),
                )
            }
            5 => (
                TermColor::Indexed(clamp_u8(self.csi.selector(index.saturating_add(2)))),
                index.saturating_add(2),
            ),
            _ => (TermColor::Default, index.saturating_add(1)),
        };
        if foreground {
            self.pen.foreground = color;
        } else {
            self.pen.background = color;
        }
        last
    }
}

/// A palette index from an SGR code offset.
fn indexed(offset: u32) -> TermColor {
    TermColor::Indexed(clamp_u8(offset))
}

/// Narrow a parameter to a palette or colour component.
fn clamp_u8(value: u32) -> u8 {
    u8::try_from(value).unwrap_or(u8::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(rows: usize, cols: usize) -> TerminalScreen {
        TerminalScreen::new(ScreenSize::new(rows, cols))
    }

    fn text(screen: &TerminalScreen, row: usize) -> String {
        String::from_utf8_lossy(&screen.render_row(row).text).into_owned()
    }

    #[test]
    fn hostile_tab_repeat_saturates_at_the_line_edge() {
        // A child-sent `CSI 4294967295 I/Z` must stop at the boundary,
        // not spin billions of iterations.
        let mut screen = screen(24, 80);
        screen.write(b"\x1b[4294967295I");
        assert_eq!(screen.cursor().col, 79);
        screen.write(b"\x1b[4294967295Z");
        assert_eq!(screen.cursor().col, 0);
    }

    #[test]
    fn printable_output_lands_on_the_first_row_with_the_cursor_below() {
        // What testterm.lua's setup_screen waits for: `tty ready` on line 1 and
        // the cursor parked on the empty line 2.
        let mut screen = screen(6, 50);
        screen.write(b"tty ready\r\n");

        assert_eq!(text(&screen, 0), "tty ready");
        assert_eq!(text(&screen, 1), "");
        assert_eq!(screen.cursor().row, 1);
        assert_eq!(screen.cursor().col, 0);
        assert_eq!(screen.line_of_row(1), 2);
    }

    #[test]
    fn trailing_untouched_cells_do_not_reach_the_line() {
        // fetch_row only advances line_len for cells with a glyph, so padding
        // after the last one never becomes buffer text (terminal.c:2335-2341).
        let mut screen = screen(2, 20);
        screen.write(b"ab");

        assert_eq!(text(&screen, 0), "ab");
    }

    #[test]
    fn cursor_byte_column_stops_at_rendered_text() {
        let mut screen = screen(2, 20);
        screen.write(b"ab\x1b[6G");
        assert_eq!(screen.cursor_byte_column(), 2);
    }

    #[test]
    fn a_written_space_is_kept_but_an_erased_one_is_not() {
        let mut screen = screen(2, 10);
        screen.write(b"a b");
        assert_eq!(text(&screen, 0), "a b");

        screen.write(b"\r\x1b[K");
        assert_eq!(text(&screen, 0), "");
    }

    #[test]
    fn control_sequence_split_across_writes_still_parses() {
        // A PTY read can end anywhere, including inside a CSI. The parser keeps
        // its state on the struct precisely so this works.
        let mut screen = screen(4, 20);
        screen.write(b"\x1b[");
        screen.write(b"3");
        screen.write(b";");
        screen.write(b"5H");
        screen.write(b"x");

        assert_eq!(screen.cursor().row, 2);
        assert_eq!(text(&screen, 2), "    x");
    }

    #[test]
    fn sgr_split_across_writes_still_colours_the_run() {
        let mut screen = screen(2, 20);
        screen.write(b"\x1b[3");
        screen.write(b"1mred\x1b[0m.");

        let rendered = screen.render_row(0);
        assert_eq!(String::from_utf8_lossy(&rendered.text), "red.");
        assert_eq!(
            rendered.spans,
            vec![AttrSpan {
                start: 0,
                end: 3,
                attrs: CellAttrs {
                    foreground: TermColor::Indexed(1),
                    background: TermColor::Default,
                    flags: CellFlags::empty(),
                },
            }]
        );
    }

    #[test]
    fn utf8_split_across_writes_produces_one_glyph() {
        let mut screen = screen(2, 20);
        screen.write(&[0xc3]);
        screen.write(&[0xa9]);

        assert_eq!(text(&screen, 0), "é");
        assert_eq!(screen.cursor().col, 1);
    }

    #[test]
    fn carriage_return_rewrites_the_current_row_in_place() {
        let mut screen = screen(2, 20);
        screen.write(b"progress 10%\rprogress 99%");

        assert_eq!(text(&screen, 0), "progress 99%");
        assert_eq!(screen.scrollback_len(), 0);
    }

    #[test]
    fn backspace_and_tab_move_within_the_row() {
        let mut screen = screen(2, 40);
        screen.write(b"ab\x08c\tz");

        assert_eq!(text(&screen, 0), "ac      z");
    }

    #[test]
    fn erase_in_display_below_clears_later_rows() {
        let mut screen = screen(4, 10);
        screen.write(b"one\r\ntwo\r\nthree\r\n");
        screen.write(b"\x1b[2;1H\x1b[J");

        assert_eq!(text(&screen, 0), "one");
        assert_eq!(text(&screen, 1), "");
        assert_eq!(text(&screen, 2), "");
    }

    #[test]
    fn erase_in_line_to_cursor_keeps_the_tail() {
        let mut screen = screen(2, 20);
        screen.write(b"abcdef\x1b[1;4H\x1b[1K");

        assert_eq!(text(&screen, 0), "    ef");
    }

    #[test]
    fn scrolling_past_the_last_row_pushes_scrollback() {
        let mut screen = screen(2, 20);
        screen.write(b"one\r\ntwo\r\nthree\r\n");

        // Two visible rows: `one` and `two` have been pushed out, `three` is on
        // the first visible row and the cursor sits on the empty second row.
        assert_eq!(screen.scrollback_len(), 2);
        assert_eq!(
            String::from_utf8_lossy(&screen.render_scrollback(0).text),
            "one"
        );
        assert_eq!(
            String::from_utf8_lossy(&screen.render_scrollback(1).text),
            "two"
        );
        assert_eq!(text(&screen, 0), "three");
        assert_eq!(text(&screen, 1), "");
        assert_eq!(screen.line_of_row(0), 3);
        assert_eq!(screen.line_count(), 4);
    }

    #[test]
    fn scrollback_is_capped_and_reports_evictions() {
        let mut screen = screen(1, 10);
        screen.set_scrollback_limit(2);
        screen.write(b"a\r\nb\r\nc\r\nd\r\n");

        assert_eq!(screen.scrollback_len(), 2);
        assert_eq!(
            String::from_utf8_lossy(&screen.render_scrollback(0).text),
            "c"
        );
        let damage = screen.take_damage().expect("output damages the screen");
        assert_eq!(damage.scrollback_deleted, 2);
    }

    #[test]
    fn alternate_screen_leaves_the_primary_content_and_scrollback_intact() {
        // altscreen_spec asserts both halves: entering must not disturb the
        // scrollback already collected, and leaving must restore the primary.
        let mut screen = screen(3, 20);
        screen.write(b"first\r\nsecond\r\nthird\r\nfourth\r\n");
        let scrollback = screen.scrollback_len();
        assert!(scrollback > 0);

        screen.write(b"\x1b[?1049h");
        assert!(screen.in_altscreen());
        screen.write(b"\x1b[HALT SCREEN");
        assert_eq!(text(&screen, 0), "ALT SCREEN");
        assert_eq!(screen.scrollback_len(), scrollback);

        screen.write(b"\x1b[?1049l");
        assert!(!screen.in_altscreen());
        assert_eq!(text(&screen, 0), "third");
        assert_eq!(text(&screen, 1), "fourth");
        assert_eq!(screen.scrollback_len(), scrollback);
    }

    #[test]
    fn scrolling_inside_the_alternate_screen_never_grows_scrollback() {
        let mut screen = screen(2, 10);
        screen.write(b"\x1b[?1049h");
        screen.write(b"a\r\nb\r\nc\r\nd\r\n");

        assert_eq!(screen.scrollback_len(), 0);
    }

    #[test]
    fn cursor_position_report_answers_with_the_real_position() {
        let mut screen = screen(6, 20);
        screen.write(b"\x1b[3;7H\x1b[6n");

        assert_eq!(screen.take_replies(), b"\x1b[3;7R".to_vec());
        assert!(screen.take_replies().is_empty());
    }

    #[test]
    fn cursor_visibility_follows_dectcem() {
        let mut screen = screen(2, 10);
        assert!(screen.cursor().visible);

        screen.write(b"\x1b[?25l");
        assert!(!screen.cursor().visible);
        screen.write(b"\x1b[?25h");
        assert!(screen.cursor().visible);
    }

    #[test]
    fn autowrap_defers_until_the_next_glyph() {
        let mut screen = screen(3, 4);
        screen.write(b"abcd");

        // The cursor stays on the last column until one more glyph arrives.
        assert_eq!(screen.cursor().row, 0);
        assert_eq!(screen.cursor().col, 3);

        screen.write(b"e");
        assert_eq!(screen.cursor().row, 1);
        assert_eq!(text(&screen, 0), "abcd");
        assert_eq!(text(&screen, 1), "e");
    }

    #[test]
    fn damage_reports_only_the_rows_that_changed() {
        let mut screen = screen(6, 20);
        let _ = screen.take_damage();

        screen.write(b"\x1b[4;1Hx");
        let damage = screen.take_damage().expect("row four changed");
        assert_eq!(damage.rows, 3..4);
        assert_eq!(damage.scrollback_pushed, 0);
        assert!(!damage.resync);
        assert!(screen.take_damage().is_none());
    }

    #[test]
    fn osc_title_sequences_do_not_reach_the_screen() {
        let mut screen = screen(2, 20);
        screen.write(b"\x1b]0;a title\x07text");

        assert_eq!(text(&screen, 0), "text");
    }

    #[test]
    fn clearing_scrollback_requests_a_resync() {
        let mut screen = screen(1, 10);
        screen.write(b"a\r\nb\r\n");
        let _ = screen.take_damage();

        screen.write(b"\x1b[3J");
        let damage = screen.take_damage().expect("scrollback was dropped");
        assert!(damage.resync);
        assert_eq!(screen.scrollback_len(), 0);
    }

    #[test]
    fn double_width_glyphs_occupy_two_columns() {
        let mut screen = screen(2, 10);
        screen.write("一二".as_bytes());

        assert_eq!(text(&screen, 0), "一二");
        assert_eq!(screen.cursor().col, 4);
    }

    #[test]
    fn insert_and_delete_lines_stay_inside_the_screen() {
        let mut screen = screen(3, 10);
        screen.write(b"a\r\nb\r\nc");
        screen.write(b"\x1b[1;1H\x1b[L");
        assert_eq!(text(&screen, 0), "");
        assert_eq!(text(&screen, 1), "a");

        screen.write(b"\x1b[1;1H\x1b[M");
        assert_eq!(text(&screen, 0), "a");
        assert_eq!(text(&screen, 1), "b");
    }

    #[test]
    fn delete_and_insert_cells_shift_the_row() {
        let mut screen = screen(2, 10);
        screen.write(b"abcdef\x1b[1;2H\x1b[2P");
        assert_eq!(text(&screen, 0), "adef");

        screen.write(b"\x1b[1;2H\x1b[2@");
        assert_eq!(text(&screen, 0), "a  def");
    }

    #[test]
    fn resize_keeps_content_and_requests_a_resync() {
        let mut screen = screen(3, 20);
        screen.write(b"keep me\r\n");
        let _ = screen.take_damage();

        screen.resize(ScreenSize::new(5, 30));
        assert_eq!(text(&screen, 0), "keep me");
        assert_eq!(screen.rows(), 5);
        assert_eq!(screen.cols(), 30);
        let damage = screen.take_damage().expect("resize damages everything");
        assert!(damage.resync);
    }
}
