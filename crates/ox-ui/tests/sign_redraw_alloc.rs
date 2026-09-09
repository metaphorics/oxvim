//! Sign-column redraw must not allocate per buffer line: a plugin that
//! places signs (gitsigns, diagnostics) redraws the same viewport whether
//! the buffer holds hundreds or hundreds of thousands of lines.
#![allow(clippy::unwrap_used)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use ox_editor::{Editor, ExtmarkPlacement, ExtmarkPosition, Geometry};
use ox_text::Buffer;
use ox_ui::{Compositor, HlState};

/// Counts requested bytes so a redraw can be measured instead of guessed.
struct Counting;

static BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

const WIDTH: usize = 120;
const HEIGHT: usize = 50;

/// A buffer of `line_count` lines carrying `sign_count` single-row signs
/// at fixed rows plus a few ranged signs — the shape a sign-heavy plugin
/// (gitsigns, diagnostics) produces. Sign positions do not depend on
/// `line_count`, so two buffers differ only in how many lines they have.
fn editor_with_signs(line_count: usize, sign_count: usize) -> Editor {
    let mut editor = Editor::new();
    let lines = vec![b"let value = compute(input);".to_vec(); line_count];
    let buffer = editor
        .create_buffer_with(Buffer::from_lines(&lines, true).unwrap(), true)
        .unwrap();
    editor
        .create_tabpage(buffer, Geometry::new(0, 0, WIDTH, HEIGHT).unwrap())
        .unwrap();
    let namespace = editor
        .buffer_mut(buffer)
        .unwrap()
        .extmarks
        .create_namespace("signs")
        .unwrap();
    {
        let extmarks = &mut editor.buffer_mut(buffer).unwrap().extmarks;
        for index in 0..sign_count {
            let row = (index * 2).min(line_count - 1);
            let mut placement = ExtmarkPlacement::new(ExtmarkPosition::new(row, 0));
            placement.attributes.sign_text = Some(">>".to_string());
            extmarks.set(namespace, None, placement).unwrap();
        }
        for index in 0..10 {
            let start = index * 97;
            let mut placement = ExtmarkPlacement::new(ExtmarkPosition::new(start, 0))
                .with_end(ExtmarkPosition::new(start + 100, 0));
            placement.attributes.sign_text = Some("!".to_string());
            extmarks.set(namespace, None, placement).unwrap();
        }
    }
    editor
}

/// Bytes allocated by one steady-state redraw: the first
/// `refresh_from_editor` builds retained layers and grids, so counting
/// starts after warm-up.
fn redraw_bytes(editor: &Editor) -> usize {
    let mut compositor = Compositor::new(WIDTH, HEIGHT);
    let mut highlights = HlState::new();
    compositor
        .refresh_from_editor(editor, WIDTH, HEIGHT, &mut highlights)
        .unwrap();
    BYTES.store(0, Ordering::Relaxed);
    compositor
        .refresh_from_editor(editor, WIDTH, HEIGHT, &mut highlights)
        .unwrap();
    BYTES.load(Ordering::Relaxed)
}

#[test]
fn sign_redraw_allocation_is_viewport_bound() {
    let small = editor_with_signs(2_000, 600);
    let large = editor_with_signs(50_000, 600);

    let small_bytes = redraw_bytes(&small);
    let large_bytes = redraw_bytes(&large);

    // Per-redraw memory tracks the viewport and the marks overlapping it,
    // not the buffer's line count.
    assert!(
        large_bytes <= small_bytes.saturating_add(64 * 1024),
        "redraw allocated {large_bytes} B for 50k lines vs {small_bytes} B for 2k lines"
    );
}
