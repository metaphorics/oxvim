//! Redraw batch packer: accumulates UI events and emits the single
//! `[2, "redraw", …]` notification frame.
//!
//! # Upstream mapping
//!
//! The outer frame is built like `src/nvim/api/ui.c` `prepare_call()` /
//! `flush_event()`: `[2, "redraw", [[event, [args]...]...]]`. Consecutive calls
//! to the same event are bundled into one `[name, args, args, …]` entry, and a
//! `grid_line` event's own arguments are `[grid, row, startcol, cells, wrap]`
//! with `cells` an array of `[char, attrid?, repeat?]` tuples
//! (`api/ui.c remote_ui_raw_line`, circa lines 817–930).
//!
//! For `grid_line` we additionally coalesce *within* the event: two adjacent
//! events for the same grid+row whose start columns are contiguous merge into a
//! single event with the cells appended, matching `remote_ui_raw_line`'s
//! practice of emitting one run of cells per line and splitting only on buffer
//! overflow (`ui_flush_buf(ui, false)` then a fresh `grid_line` at the same
//! position). The merged event keeps the first startcol and takes the final
//! `wrap` flag (upstream sets `wrap` only on the final `grid_line` for a line).
//!
//! The coalescer is pure data rearrangement (no I/O) so `ox-ui` can drive it.

use ox_types::{Object, OxStr};

/// A bundled event within a redraw batch: `[name, argset1, argset2, …]`.
#[derive(Debug, Clone, PartialEq)]
pub struct RedrawEvent {
    /// Event name (e.g. `"grid_line"`).
    pub name: OxStr,
    /// The argument sets bundled under `name`; each is one method call.
    pub argsets: Vec<Vec<Object>>,
}

/// Accumulates UI events and can pack them into a `"redraw"` notification.
#[derive(Debug, Default, Clone)]
pub struct RedrawBatch {
    events: Vec<RedrawEvent>,
}

/// Index of the `grid_line` args layout `[grid, row, startcol, cells, wrap]`.
const ARG_GRID: usize = 0;
const ARG_ROW: usize = 1;
const ARG_STARTCOL: usize = 2;
const ARG_CELLS: usize = 3;
const ARG_WRAP: usize = 4;

impl RedrawBatch {
    /// An empty batch.
    #[must_use]
    pub fn new() -> Self {
        Self { events: Vec::new() }
    }

    /// Append an argument set to an event, bundling consecutive same-name calls
    /// into one event entry (upstream `prepare_call`). If the last event has a
    /// different `name` a new event entry is started.
    pub fn push(&mut self, name: impl Into<OxStr>, args: Vec<Object>) {
        let name = name.into();
        if let Some(last) = self.events.last_mut() {
            if last.name == name {
                last.argsets.push(args);
                return;
            }
        }
        self.events.push(RedrawEvent { name, argsets: vec![args] });
    }

    /// Append a `grid_line` call, coalescing it into the previous call when it
    /// targets the same grid+row at a contiguous start column.
    ///
    /// `cells` is the array of `[char, attrid?, repeat?]` cell tuples emitted
    /// verbatim on the wire.
    pub fn grid_line(
        &mut self,
        grid: i64,
        row: i64,
        startcol: i64,
        cells: Vec<Object>,
        wrap: bool,
    ) {
        // Try to fuse with the previous grid_line (contiguous same grid+row).
        if let Some(last) = self.events.last_mut() {
            if last.name == OxStr::from("grid_line") {
                if let Some(prev) = last.argsets.last_mut() {
                    if can_fuse(prev, grid, row, startcol) {
                        // Append the new cell tuples and adopt the final wrap.
                        if let Object::Array(prev_cells) = &mut prev[ARG_CELLS] {
                            prev_cells.extend(cells);
                        }
                        prev[ARG_WRAP] = Object::Boolean(wrap);
                        return;
                    }
                }
            }
        }
        self.push(
            "grid_line",
            vec![
                Object::Integer(grid),
                Object::Integer(row),
                Object::Integer(startcol),
                Object::Array(cells),
                Object::Boolean(wrap),
            ],
        );
    }

    /// The events collected so far (empty when nothing has been pushed).
    #[must_use]
    pub fn events(&self) -> &[RedrawEvent] {
        &self.events
    }

    /// Whether the batch holds no events.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Pack the batch into the single `[2, "redraw", …]` notification frame.
    ///
    /// A batch of calls is emitted as one event entry per name —
    /// `[name, args1, args2, …]` — matching `flush_event`'s `1 + ncalls`
    /// accounting.
    pub fn pack(&self) -> Vec<u8> {
        let events: Vec<Object> = self
            .events
            .iter()
            .map(|event| {
                let mut entry: Vec<Object> = vec![Object::String(event.name.clone())];
                entry.extend(event.argsets.iter().map(|args| Object::Array(args.clone())));
                Object::Array(entry)
            })
            .collect();
        let frame = Object::Array(vec![
            Object::Integer(2),
            Object::String(OxStr::from("redraw")),
            Object::Array(events),
        ]);
        crate::codec::encode(&frame)
    }
}

/// Number of cells a cell tuple `[char, attrid?, repeat?]` represents
/// (`remote_ui_raw_line`: `repeat` defaults to 1).
fn cell_count(tuple: &Object) -> i64 {
    if let Object::Array(cells) = tuple {
        if let Some(Object::Integer(repeat)) = cells.get(2) {
            return *repeat;
        }
    }
    1
}

/// If `prev` is a `grid_line` for the same grid+row that ends exactly at
/// `startcol`, return whether it is safe to fuse the new cells into it.
fn can_fuse(prev: &[Object], grid: i64, row: i64, startcol: i64) -> bool {
    if prev.len() != 5 {
        return false;
    }
    let matches_int = |v: &Object, want: i64| matches!(v, Object::Integer(n) if *n == want);
    if !matches_int(&prev[ARG_GRID], grid) || !matches_int(&prev[ARG_ROW], row) {
        return false;
    }
    let Object::Integer(prev_start) = prev[ARG_STARTCOL] else {
        return false;
    };
    let Object::Array(cells) = &prev[ARG_CELLS] else {
        return false;
    };
    let covered = cells
        .iter()
        .try_fold(0i64, |acc, tuple| acc.checked_add(cell_count(tuple)));
    let Some(covered) = covered else {
        return false;
    };
    let Some(end) = prev_start.checked_add(covered) else {
        return false;
    };
    end == startcol
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::io::Cursor;
    use rmpv::Value;

    fn parse(bytes: &[u8]) -> Value {
        let mut cur = Cursor::new(bytes);
        rmpv::decode::read_value(&mut cur).unwrap()
    }

    fn cell(ch: &str, attr: i64, repeat: i64) -> Object {
        Object::Array(vec![
            Object::String(OxStr::from(ch)),
            Object::Integer(attr),
            Object::Integer(repeat),
        ])
    }

    #[test]
    fn same_grid_row_coalesces() {
        let mut b = RedrawBatch::new();
        b.grid_line(1, 3, 0, vec![cell("a", 0, 1), cell("b", 1, 1)], false);
        b.grid_line(1, 3, 2, vec![cell("c", 2, 5)], true);
        assert_eq!(b.events().len(), 1);
        // One bundled event, one fused argset covering all cells.
        let e = &b.events()[0];
        assert_eq!(e.name, OxStr::from("grid_line"));
        assert_eq!(e.argsets.len(), 1);
        let args = &e.argsets[0];
        assert_eq!(args[ARG_GRID], Object::Integer(1));
        assert_eq!(args[ARG_ROW], Object::Integer(3));
        assert_eq!(args[ARG_STARTCOL], Object::Integer(0));
        assert_eq!(args[ARG_WRAP], Object::Boolean(true)); // final wrap wins
        match &args[ARG_CELLS] {
            Object::Array(cells) => assert_eq!(cells.len(), 3),
            _ => panic!("cells must be an array"),
        }
    }

    #[test]
    fn different_rows_do_not_coalesce() {
        let mut b = RedrawBatch::new();
        b.grid_line(1, 3, 0, vec![cell("a", 0, 1)], false);
        b.grid_line(1, 4, 0, vec![cell("b", 0, 1)], false);
        assert_eq!(b.events().len(), 1); // still one "grid_line" event entry
        assert_eq!(b.events()[0].argsets.len(), 2); // but two argument sets
    }

    #[test]
    fn gap_prevents_coalesce() {
        let mut b = RedrawBatch::new();
        b.grid_line(1, 3, 0, vec![cell("a", 0, 1), cell("b", 1, 1)], false);
        b.grid_line(1, 3, 5, vec![cell("c", 2, 1)], false); // gap at col 2..5
        assert_eq!(b.events()[0].argsets.len(), 2);
    }

    #[test]
    fn overflow_startcol_returns_false() {
        let mut b = RedrawBatch::new();
        // prev_start = i64::MAX and one cell of width 1 would overflow when
        // computing the end column; can_fuse must return false, not panic.
        b.grid_line(1, 3, i64::MAX, vec![cell("a", 0, 1)], false);
        b.grid_line(1, 3, i64::MAX, vec![cell("b", 0, 1)], false);
        assert_eq!(b.events()[0].argsets.len(), 2);
    }

    #[test]
    fn pack_emits_redraw_notification_shape() {
        let mut b = RedrawBatch::new();
        b.grid_line(1, 0, 0, vec![cell("x", 0, 1)], false);
        b.push("flush", vec![]);
        let bytes = b.pack();
        // Decode the frame and check the shape.
        let v = parse(&bytes);
        let Value::Array(frame) = v else { panic!("top must be array") };
        assert_eq!(frame.len(), 3);
        assert_eq!(frame[0].as_i64(), Some(2));
        assert_eq!(frame[1].as_str(), Some("redraw"));
        let Value::Array(events) = &frame[2] else { panic!("events must be array") };
        assert_eq!(events.len(), 2); // one grid_line, one flush
        // grid_line entry: ["grid_line", [args...]]
        let Value::Array(ge) = &events[0] else { panic!() };
        assert_eq!(ge[0].as_str(), Some("grid_line"));
        assert_eq!(ge.len(), 2);
        // flush entry: ["flush", []]
        let Value::Array(fe) = &events[1] else { panic!() };
        assert_eq!(fe[0].as_str(), Some("flush"));
        assert_eq!(fe.len(), 2);
        assert_eq!(fe[1].as_array().map(|s| s.len()), Some(0));
    }

    #[test]
    fn pack_reparses_via_decoder() {
        let mut b = RedrawBatch::new();
        b.grid_line(1, 0, 0, vec![cell("x", 0, 1)], false);
        let bytes = b.pack();
        // Whole frame is a valid msgpack value (a notification "redraw").
        let obj = crate::codec::decode(&bytes).unwrap();
        assert!(matches!(obj, Object::Array(_)));
    }
}
