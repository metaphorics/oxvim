//! Default tabline row, ported from upstream `draw_tabline`
//! (`.references/neovim/src/nvim/statusline.c:583-752`) gated on
//! `tabline_height` (`window.c:7416-7429`).
//!
//! Pure layout: the functions take the tab pages, the current tab page, the
//! available width and the relevant options, and return the row's cells with
//! their highlight groups. No global state, no rendering side effects — the
//! compositor paints the returned runs onto the default grid's top row and
//! reserves the row by shrinking the window area.

use ox_editor::{BufferFlags, Editor, OptionValue};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::hl::{Highlight, HlAttrs};

/// Highlight group a tabline run draws with (`highlight.h:68-70` maps the
/// `HLF_TP`/`HLF_TPS`/`HLF_TPF` slots to these names; `HLF_T` is `Title`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TablineHl {
    /// `TabLine` — non-current tab labels and the `X` close button
    /// (`attr_nosel`, statusline.c:586,738).
    Tab,
    /// `TabLineSel` — the current tab's label (statusline.c:641-643).
    TabSel,
    /// `TabLineFill` — filler after the last label (`attr_fill`,
    /// statusline.c:587,722).
    Fill,
    /// `Title` combined over the enclosing tab's attr — the window-count
    /// digit (`hl_combine_attr(attr, win_hl_attr(cwp, HLF_T))`,
    /// statusline.c:671). Resolve as `HlState::combine(tab_id, title_id)`
    /// where `tab_id` is the enclosing [`TablineHl::Tab`]/[`TablineHl::TabSel`]
    /// run's id; that run always immediately precedes this cell.
    Count,
}

impl TablineHl {
    /// The highlight group name the run resolves through.
    #[must_use]
    pub const fn group_name(self) -> &'static str {
        match self {
            Self::Tab => "TabLine",
            Self::TabSel => "TabLineSel",
            Self::Fill => "TabLineFill",
            Self::Count => "Title",
        }
    }

    /// The resolved default attributes, matching the reference binary's
    /// startup table. The spec's `screen:expect` attr keys map by resolved
    /// attrs, not by group id, so these are the values the emitted
    /// `hl_attr_define` must carry. Prefer `HlState::group_id` (a
    /// `nvim_set_hl` override) and fall back to this when the group was never
    /// defined on the render state.
    ///
    /// Note: the reference *binary* predates the source tree's
    /// `default link TabLine StatusLineNC` (highlight_group.c:191-192); its
    /// `TabLine`/`TabLineFill` carry the classic direct defaults below, which
    /// is what `tabpage_spec.lua`'s expected rows encode.
    #[must_use]
    pub fn default_highlight(self) -> Highlight {
        match self {
            // TabLine: `gui=underline guibg=LightGrey` /
            // `cterm=underline ctermfg=0 ctermbg=7`.
            Self::Tab => Highlight {
                rgb: HlAttrs {
                    background: Some(0x00d3_d3d3),
                    underline: true,
                    ..HlAttrs::default()
                },
                cterm: HlAttrs {
                    foreground: Some(0),
                    background: Some(7),
                    underline: true,
                    fg_indexed: true,
                    bg_indexed: true,
                    ..HlAttrs::default()
                },
                cterm_explicit: true,
                ..Highlight::default()
            },
            // TabLineSel: `gui=bold` (`guifg=fg guibg=bg` resolve to the
            // default colors and are omitted from the emitted attrs).
            Self::TabSel => Highlight {
                rgb: HlAttrs {
                    bold: true,
                    ..HlAttrs::default()
                },
                cterm: HlAttrs {
                    bold: true,
                    ..HlAttrs::default()
                },
                cterm_explicit: true,
                ..Highlight::default()
            },
            // TabLineFill: `term=reverse cterm=reverse gui=reverse`.
            Self::Fill => Highlight {
                rgb: HlAttrs {
                    reverse: true,
                    ..HlAttrs::default()
                },
                cterm: HlAttrs {
                    reverse: true,
                    ..HlAttrs::default()
                },
                cterm_explicit: true,
                ..Highlight::default()
            },
            // Title (the window-count digit): bold magenta.
            Self::Count => Highlight {
                rgb: HlAttrs {
                    foreground: Some(0x00ff_00ff),
                    bold: true,
                    ..HlAttrs::default()
                },
                cterm: HlAttrs {
                    foreground: Some(5),
                    bold: true,
                    fg_indexed: true,
                    ..HlAttrs::default()
                },
                cterm_explicit: true,
                ..Highlight::default()
            },
        }
    }
}

/// Per-tabpage input for the default tabline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TablineTab {
    /// Display name of the tab's current window's buffer: the buffer name
    /// after `buf_spname`/`home_replace` (statusline.c:130-138). An empty
    /// string means unnamed and is drawn as `[No Name]`
    /// (`buf_get_fname`, buffer.c:4156-4164).
    pub name: String,
    /// Any window in the tabpage shows a modified buffer
    /// (`bufIsChanged`, statusline.c:660).
    pub modified: bool,
    /// Windows in the tabpage. Upstream counts only `focusable` non-`hide`
    /// windows (statusline.c:656-661); oxvim's `WinConfig` has no such flags
    /// yet, so every window counts.
    pub window_count: usize,
    /// This tabpage is the current one (`tp == curtab`, statusline.c:633).
    pub current: bool,
}

/// One labelled run of cells on the tabline row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TablineCell {
    /// Starting column on the default grid's top row.
    pub col: usize,
    /// Text drawn from `col`.
    pub text: String,
    /// Highlight group for these cells.
    pub hl: TablineHl,
}

/// Everything the compositor needs to lay out and paint the tabline: the
/// rows it reserves at the top of the default grid and the cell runs to
/// paint there.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TablineLayout {
    /// Rows reserved at the top of the default grid (0 or 1).
    pub height: usize,
    /// Labelled cell runs for the top row, in draw order. Later runs may
    /// overwrite earlier cells — the `X` close button lands on the filler's
    /// last cell (statusline.c:737-744).
    pub cells: Vec<TablineCell>,
}

/// `tabline_height` (window.c:7416-7429): rows the tabline reserves at the
/// top of the default grid.
///
/// `ext_tabline` UIs take the `tabline_update` event instead of the grid row
/// (statusline.c:595-598); ox-ui does not negotiate `ext_tabline` yet, so the
/// caller passes `false` until [`crate::UiOptions`] grows the capability.
///
/// `showtabline` is the `'showtabline'` option value: `0` never shows, `1`
/// shows only when at least two tab pages exist, `2` (and any other value)
/// always shows.
#[must_use]
pub fn tabline_height(showtabline: i64, tabpage_count: usize, ext_tabline: bool) -> usize {
    if ext_tabline {
        return 0;
    }
    match showtabline {
        0 => 0,
        1 => usize::from(tabpage_count > 1),
        _ => 1,
    }
}

/// Collects the default-tabline inputs for every tabpage in order
/// (`FOR_ALL_TABS`, statusline.c:617,626).
///
/// The label comes from the tab's current window's buffer: `curwin` for the
/// current tabpage, `tp_curwin` for the others (statusline.c:633-639) — both
/// are the tabpage's own current window here.
#[must_use]
pub fn collect_tabs(editor: &Editor) -> Vec<TablineTab> {
    let current = editor.current_tabpage();
    let mut tabs = Vec::new();
    for handle in editor.tabpages() {
        let Ok(tab) = editor.tabpage(handle) else {
            continue;
        };
        let window = tab.current_window();
        let name = editor
            .window(window)
            .ok()
            .and_then(|state| editor.buffer(state.buffer).ok())
            .map_or(String::new(), |buffer| {
                String::from_utf8_lossy(buffer.name().as_bytes()).into_owned()
            });
        let windows = tab.windows();
        let modified = windows.iter().any(|window| {
            editor
                .window(*window)
                .ok()
                .and_then(|state| editor.buffer(state.buffer).ok())
                .is_some_and(|buffer| buffer.flags.contains(BufferFlags::MODIFIED))
        });
        tabs.push(TablineTab {
            name,
            modified,
            window_count: windows.len(),
            current: current == Some(handle),
        });
    }
    tabs
}

/// `draw_tabline`'s default path (statusline.c:611-746): the row's cells.
///
/// Call only when [`tabline_height`] is non-zero. The `'tabline'` option
/// path (`*p_tal != NUL` → `win_redr_stl_expr`, statusline.c:609-611) is not
/// covered: oxvim has no `'tabline'` option registered and no statusline
/// expression evaluator reachable from this crate (the shared grammar lives
/// in `ox-api`'s `nvim_eval_statusline`, whose `%T`/`%X` items still render
/// empty — global.rs:3398-3402). When that lands, the caller branches on the
/// option before calling here.
#[must_use]
pub fn build_tabline(tabs: &[TablineTab], width: usize) -> Vec<TablineCell> {
    let mut row = RowBuilder::new(width);
    let total = tabs.len();
    // statusline.c:621 — each tab's column budget.
    let tabwidth = ((width.saturating_sub(1) + total / 2) / total.max(1)).max(6);
    let mut drawn = 0usize;
    for tab in tabs {
        // statusline.c:627 — stop when fewer than four columns remain.
        if row.col + 4 >= width {
            break;
        }
        let scol = row.col;
        // statusline.c:641-650 — current tab draws TabLineSel, the rest
        // TabLine.
        let hl = if tab.current {
            TablineHl::TabSel
        } else {
            TablineHl::Tab
        };
        // statusline.c:652 — leading space.
        row.put(" ", hl);
        // statusline.c:664-678 — window count and modified marker.
        if tab.modified || tab.window_count > 1 {
            if tab.window_count > 1 {
                let digits = tab.window_count.to_string();
                // statusline.c:667 — bail when the count cannot fit.
                if row.col + digits.len() + 3 >= width {
                    break;
                }
                row.put(&digits, TablineHl::Count);
            }
            if tab.modified {
                row.put("+", hl);
            }
            row.put(" ", hl);
        }
        // statusline.c:680 — room left for the name inside this tab's budget.
        let room = (scol + tabwidth).saturating_sub(row.col + 1);
        if room > 0 {
            // statusline.c:683-684 — buffer name, directories shortened.
            let name = display_name(&tab.name);
            // statusline.c:687-690 — drop leading characters until the name
            // fits the budget, keeping the tail.
            let name = truncate_left(&name, room);
            // statusline.c:691-692 — never let the label reach the last
            // column; the trailing space (or the close button) owns it.
            let budget = width.saturating_sub(row.col + 1);
            let name = first_cells(name, budget);
            row.put(name, hl);
        }
        // statusline.c:697 — trailing space.
        row.put(" ", hl);
        drawn += 1;
    }
    // statusline.c:721-722 — filler to the screen edge.
    row.fill_to(width, TablineHl::Fill);
    // statusline.c:737-744 — the close button only when more than one tab
    // was drawn.
    if drawn > 1 {
        row.put_at(width.saturating_sub(1), "X", TablineHl::Tab);
    }
    row.cells
}

/// `draw_tabline` gated on `tabline_height`: the row plus the height it
/// reserves, or nothing when `'showtabline'` hides it.
///
/// This is the single integration entry: the compositor calls it once, uses
/// `height` to offset and shrink the window layers, and paints `cells` onto
/// the default grid's top row.
#[must_use]
pub fn tabline_layout(editor: &Editor, width: usize) -> TablineLayout {
    // 'showtabline' defaults to 1 (options.lua); it is not a registered
    // option yet, so an unregistered read falls back to the default.
    let showtabline = match editor.options().get_global("showtabline") {
        Ok(OptionValue::Number(value)) => *value,
        _ => 1,
    };
    let tabs = collect_tabs(editor);
    let height = tabline_height(showtabline, tabs.len(), false);
    let cells = if height == 0 {
        Vec::new()
    } else {
        build_tabline(&tabs, width)
    };
    TablineLayout { height, cells }
}

/// The buffer's display name for the tabline: `[No Name]` when unnamed
/// (`buf_get_fname`, buffer.c:4156-4164), else the path with each directory
/// component shortened (`shorten_dir`, statusline.c:684 → path.c:341).
fn display_name(name: &str) -> String {
    if name.is_empty() {
        "[No Name]".to_owned()
    } else {
        shorten_dir(name)
    }
}

/// `shorten_dir` (path.c:304-344): shorten each directory component to one
/// character, keeping the final component whole — `"~/foo/../.bar/fname"`
/// becomes `"~/f/../.b/fname"`.
fn shorten_dir(name: &str) -> String {
    // `path_tail` — the final component starts after the last separator.
    let tail = name.rfind('/').map_or(0, |index| index + 1);
    let mut out = String::with_capacity(name.len());
    let mut skip = false;
    let mut dirchunk_len = 0usize;
    for (index, ch) in name.char_indices() {
        if index >= tail {
            // Copy the whole tail.
            out.push(ch);
        } else if ch == '/' {
            // Copy the separator and restart the next component.
            out.push(ch);
            skip = false;
            dirchunk_len = 0;
        } else if !skip {
            out.push(ch);
            // Leading `~` and `.` do not count toward the kept length.
            if ch != '~' && ch != '.' {
                dirchunk_len += 1;
                if dirchunk_len >= 1 {
                    skip = true;
                }
            }
        }
    }
    out
}

/// Drops leading characters until the display width fits `room`, keeping the
/// tail (statusline.c:687-690).
fn truncate_left(name: &str, room: usize) -> &str {
    let mut width = UnicodeWidthStr::width(name);
    let mut start = 0;
    for ch in name.chars() {
        if width <= room {
            break;
        }
        width = width.saturating_sub(UnicodeWidthChar::width(ch).unwrap_or(0));
        start += ch.len_utf8();
    }
    &name[start..]
}

/// The longest prefix of `text` whose display width fits `max` cells.
fn first_cells(text: &str, max: usize) -> &str {
    let mut width = 0;
    let mut end = 0;
    for ch in text.chars() {
        let cell = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + cell > max {
            break;
        }
        width += cell;
        end += ch.len_utf8();
    }
    &text[..end]
}

/// Accumulates the row's cell runs at a running column, merging adjacent
/// runs that share a highlight.
struct RowBuilder {
    cells: Vec<TablineCell>,
    col: usize,
    width: usize,
}

impl RowBuilder {
    fn new(width: usize) -> Self {
        Self {
            cells: Vec::new(),
            col: 0,
            width,
        }
    }

    /// Appends `text` at the running column, clipped to the grid edge.
    fn put(&mut self, text: &str, hl: TablineHl) {
        let shown = first_cells(text, self.width.saturating_sub(self.col));
        let width = UnicodeWidthStr::width(shown);
        if width == 0 {
            return;
        }
        if let Some(last) = self.cells.last_mut()
            && last.hl == hl
            && last.col + UnicodeWidthStr::width(last.text.as_str()) == self.col
        {
            last.text.push_str(shown);
        } else {
            self.cells.push(TablineCell {
                col: self.col,
                text: shown.to_owned(),
                hl,
            });
        }
        self.col += width;
    }

    /// Fills from the running column to `end` with spaces.
    fn fill_to(&mut self, end: usize, hl: TablineHl) {
        if end > self.col {
            self.put(&" ".repeat(end - self.col), hl);
        }
    }

    /// Writes `text` at an absolute column as its own run — the close button
    /// lands on the filler's last cell and must be emitted after it.
    fn put_at(&mut self, col: usize, text: &str, hl: TablineHl) {
        self.cells.push(TablineCell {
            col,
            text: text.to_owned(),
            hl,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(name: &str, current: bool) -> TablineTab {
        TablineTab {
            name: name.to_owned(),
            modified: false,
            window_count: 1,
            current,
        }
    }

    /// Renders the cells into a `(text, hl)` map per column for assertions.
    fn render(cells: &[TablineCell], width: usize) -> Vec<(char, TablineHl)> {
        let mut row = vec![(' ', TablineHl::Fill); width];
        for cell in cells {
            let mut col = cell.col;
            for ch in cell.text.chars() {
                if col >= width {
                    break;
                }
                row[col] = (ch, cell.hl);
                col += UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
            }
        }
        row
    }

    fn text(row: &[(char, TablineHl)]) -> String {
        row.iter().map(|(ch, _)| ch).collect()
    }

    // tabpage_spec.lua:234-239 — two tabs, the first current.
    #[test]
    fn two_tabs_current_first() {
        let tabs = vec![tab("", true), tab("", false)];
        let cells = build_tabline(&tabs, 50);
        let row = render(&cells, 50);
        assert_eq!(
            text(&row),
            " [No Name]  [No Name]                            X"
        );
        assert_eq!(row[0].1, TablineHl::TabSel);
        assert_eq!(row[10].1, TablineHl::TabSel);
        assert_eq!(row[11].1, TablineHl::Tab);
        assert_eq!(row[21].1, TablineHl::Tab);
        assert_eq!(row[22].1, TablineHl::Fill);
        assert_eq!(row[48].1, TablineHl::Fill);
        assert_eq!(row[49], ('X', TablineHl::Tab));
    }

    // tabpage_spec.lua:247-252 — three tabs, the middle one current.
    #[test]
    fn three_tabs_current_middle() {
        let tabs = vec![tab("", false), tab("", true), tab("", false)];
        let cells = build_tabline(&tabs, 50);
        let row = render(&cells, 50);
        assert_eq!(
            text(&row),
            " [No Name]  [No Name]  [No Name]                 X"
        );
        assert_eq!(row[0].1, TablineHl::Tab);
        assert_eq!(row[11].1, TablineHl::TabSel);
        assert_eq!(row[22].1, TablineHl::Tab);
        assert_eq!(row[33].1, TablineHl::Fill);
        assert_eq!(row[49], ('X', TablineHl::Tab));
    }

    // tabpage_spec.lua:256-261 — four tabs, the second current.
    #[test]
    fn four_tabs_current_second() {
        let tabs = vec![
            tab("", false),
            tab("", true),
            tab("", false),
            tab("", false),
        ];
        let cells = build_tabline(&tabs, 50);
        let row = render(&cells, 50);
        assert_eq!(
            text(&row),
            " [No Name]  [No Name]  [No Name]  [No Name]      X"
        );
        assert_eq!(row[11].1, TablineHl::TabSel);
        assert_eq!(row[44].1, TablineHl::Fill);
        assert_eq!(row[49], ('X', TablineHl::Tab));
    }

    // window.c:7416-7429 — 'showtabline' gating.
    #[test]
    fn showtabline_gating() {
        assert_eq!(tabline_height(0, 3, false), 0);
        assert_eq!(tabline_height(1, 1, false), 0);
        assert_eq!(tabline_height(1, 2, false), 1);
        assert_eq!(tabline_height(2, 1, false), 1);
        // An ext_tabline UI takes the event instead of the grid row.
        assert_eq!(tabline_height(2, 3, true), 0);
    }

    // statusline.c:664-678 — the window-count digit and modified marker.
    #[test]
    fn window_count_and_modified() {
        let mut tabs = vec![tab("", true), tab("", false)];
        tabs[1].window_count = 3;
        tabs[1].modified = true;
        let cells = build_tabline(&tabs, 50);
        let row = render(&cells, 50);
        // Second tab: ` 3+ [No Name] `.
        assert_eq!(text(&row)[11..25].to_owned(), " 3+ [No Name] ");
        assert_eq!(row[12].1, TablineHl::Count);
        assert_eq!(row[13], ('+', TablineHl::Tab));
    }

    // statusline.c:627 — tabs stop drawing when fewer than four columns
    // remain; the rest collapse into the filler.
    #[test]
    fn overflow_truncates() {
        let tabs = vec![tab("", true); 20];
        let cells = build_tabline(&tabs, 30);
        let row = render(&cells, 30);
        // Every label is clipped; the row never exceeds the width and the
        // close button still lands on the last column.
        assert_eq!(row[29], ('X', TablineHl::Tab));
        assert!(cells.iter().all(|cell| cell.col < 30));
    }

    // path.c:304-344 — directory components shorten to one character.
    #[test]
    fn shortens_directories() {
        assert_eq!(shorten_dir("~/foo/../.bar/fname"), "~/f/../.b/fname");
        assert_eq!(shorten_dir("/home/user/file.txt"), "/h/u/file.txt");
        assert_eq!(shorten_dir("plain.txt"), "plain.txt");
    }

    // buffer.c:4156-4164 — the unnamed-buffer label.
    #[test]
    fn unnamed_is_no_name() {
        assert_eq!(display_name(""), "[No Name]");
        assert_eq!(display_name("/a/b/c.txt"), "/a/b/c.txt");
    }
}
