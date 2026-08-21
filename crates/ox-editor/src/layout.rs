//! Tiled and floating window layout state.
//!
//! A [`Layout`] owns a recursive frame tree. `Row` frames place children from
//! left to right, while `Column` frames place them from top to bottom. Window
//! numbers are derived from a stable, one-based preorder traversal; handles,
//! rather than numbers, remain the persistent identity.

use ox_text::Position;
use ox_types::{BufHandle, WinHandle};
use thiserror::Error;

/// A rectangular screen region.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Geometry {
    /// Zero-based screen row.
    pub row: usize,
    /// Zero-based screen column.
    pub col: usize,
    /// Width in screen cells.
    pub width: usize,
    /// Height in screen cells.
    pub height: usize,
}

impl Geometry {
    /// Creates a non-empty screen region.
    pub fn new(row: usize, col: usize, width: usize, height: usize) -> Result<Self, LayoutError> {
        if width == 0 || height == 0 {
            return Err(LayoutError::InvalidDimensions { width, height });
        }
        if row.checked_add(height).is_none() || col.checked_add(width).is_none() {
            return Err(LayoutError::GeometryOverflow);
        }
        Ok(Self {
            row,
            col,
            width,
            height,
        })
    }
}

/// Buffer and viewport state displayed by a window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowState {
    /// Buffer displayed by the window.
    pub buffer: BufHandle,
    /// Cursor position in the buffer.
    pub cursor: Position,
    /// One-based first buffer line displayed by the window.
    pub topline: usize,
}

impl WindowState {
    /// Creates window state with a normalized one-based top line.
    #[must_use]
    pub fn new(buffer: BufHandle, cursor: Position) -> Self {
        Self {
            buffer,
            cursor,
            topline: 1,
        }
    }
}

/// A leaf in the tiled frame tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeafFrame {
    /// Stable window identity.
    pub window: WinHandle,
    /// Buffer and viewport state.
    pub state: WindowState,
    /// Region assigned by the parent frame.
    pub geometry: Geometry,
}

/// A recursive tiled frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Frame {
    /// A window occupying one rectangle.
    Leaf(LeafFrame),
    /// Children arranged from left to right.
    Row {
        /// Region shared by the children.
        geometry: Geometry,
        /// Child frames in display and preorder order.
        children: Vec<Frame>,
    },
    /// Children arranged from top to bottom.
    Column {
        /// Region shared by the children.
        geometry: Geometry,
        /// Child frames in display and preorder order.
        children: Vec<Frame>,
    },
}

impl Frame {
    /// Returns this frame's assigned rectangle.
    #[must_use]
    pub const fn geometry(&self) -> Geometry {
        match self {
            Self::Leaf(leaf) => leaf.geometry,
            Self::Row { geometry, .. } | Self::Column { geometry, .. } => *geometry,
        }
    }

    /// Returns the number of tiled windows below this frame.
    #[must_use]
    pub fn window_count(&self) -> usize {
        match self {
            Self::Leaf(_) => 1,
            Self::Row { children, .. } | Self::Column { children, .. } => {
                children.iter().map(Self::window_count).sum()
            }
        }
    }
}

/// Failures from layout and window operations.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum LayoutError {
    /// The requested window is not present in the tabpage.
    #[error("unknown window {0:?}")]
    UnknownWindow(WinHandle),
    /// The requested one-based window number is not present.
    #[error("unknown window number {0}")]
    UnknownWindowNumber(usize),
    /// A real window identity cannot use the current-window sentinel.
    #[error("the current-window sentinel cannot be stored as a window identity")]
    CurrentWindow,
    /// Closing the sole tiled window would leave the layout without a root.
    #[error("cannot close the last tiled window")]
    LastWindow,
    /// A window handle already belongs to this tabpage.
    #[error("window {0:?} already belongs to the tabpage")]
    DuplicateWindow(WinHandle),
    /// Screen rectangles and window configurations must be non-empty.
    #[error("invalid window dimensions {width}x{height}")]
    InvalidDimensions {
        /// Requested width.
        width: usize,
        /// Requested height.
        height: usize,
    },
    /// A split cannot give every resulting frame at least one cell.
    #[error("{available} cells cannot be divided among {children} frame children")]
    InsufficientSpace {
        /// Available cells on the split axis.
        available: usize,
        /// Number of children sharing the axis.
        children: usize,
    },
    /// A floating-window coordinate must be finite.
    #[error("floating-window row and column must be finite")]
    InvalidCoordinate,
    /// Geometry arithmetic exceeded the platform coordinate range.
    #[error("window geometry exceeds the supported coordinate range")]
    GeometryOverflow,
}

/// A tiled window layout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Layout {
    root: Frame,
    current: WinHandle,
}

impl Layout {
    /// Creates a layout containing one tiled window.
    pub fn new(
        window: WinHandle,
        state: WindowState,
        geometry: Geometry,
    ) -> Result<Self, LayoutError> {
        validate_identity(window)?;
        validate_geometry(geometry)?;
        Ok(Self {
            root: Frame::Leaf(LeafFrame {
                window,
                state,
                geometry,
            }),
            current: window,
        })
    }

    /// Returns the root of the tiled frame tree.
    #[must_use]
    pub const fn root(&self) -> &Frame {
        &self.root
    }

    /// Returns the current tiled window.
    #[must_use]
    pub const fn current_window(&self) -> WinHandle {
        self.current
    }

    /// Returns the number of tiled windows.
    #[must_use]
    pub fn window_count(&self) -> usize {
        self.root.window_count()
    }

    /// Makes a tiled window current. [`WinHandle::CURRENT`] resolves to the
    /// already-current window.
    pub fn set_current(&mut self, window: WinHandle) -> Result<(), LayoutError> {
        let resolved = self.resolve(window);
        if find_leaf(&self.root, resolved).is_none() {
            return Err(LayoutError::UnknownWindow(resolved));
        }
        self.current = resolved;
        Ok(())
    }

    /// Returns immutable state for a tiled window.
    pub fn window(&self, window: WinHandle) -> Result<&WindowState, LayoutError> {
        let resolved = self.resolve(window);
        find_leaf(&self.root, resolved)
            .map(|leaf| &leaf.state)
            .ok_or(LayoutError::UnknownWindow(resolved))
    }

    /// Returns mutable state for a tiled window.
    pub fn window_mut(&mut self, window: WinHandle) -> Result<&mut WindowState, LayoutError> {
        let resolved = self.resolve(window);
        find_leaf_mut(&mut self.root, resolved)
            .map(|leaf| &mut leaf.state)
            .ok_or(LayoutError::UnknownWindow(resolved))
    }

    /// Returns the assigned geometry for a tiled window.
    pub fn window_geometry(&self, window: WinHandle) -> Result<Geometry, LayoutError> {
        let resolved = self.resolve(window);
        find_leaf(&self.root, resolved)
            .map(|leaf| leaf.geometry)
            .ok_or(LayoutError::UnknownWindow(resolved))
    }

    /// Returns a window's stable one-based preorder number.
    pub fn winnr(&self, window: WinHandle) -> Result<usize, LayoutError> {
        let resolved = self.resolve(window);
        let mut next = 1;
        preorder_number(&self.root, resolved, &mut next)
            .ok_or(LayoutError::UnknownWindow(resolved))
    }

    /// Resolves a one-based preorder number to a stable handle.
    pub fn window_by_winnr(&self, winnr: usize) -> Result<WinHandle, LayoutError> {
        if winnr == 0 {
            return Err(LayoutError::UnknownWindowNumber(winnr));
        }
        let mut next = 1;
        preorder_window(&self.root, winnr, &mut next)
            .ok_or(LayoutError::UnknownWindowNumber(winnr))
    }

    /// Returns tiled window handles in preorder.
    #[must_use]
    pub fn windows(&self) -> Vec<WinHandle> {
        let mut windows = Vec::with_capacity(self.window_count());
        collect_windows(&self.root, &mut windows);
        windows
    }

    /// Splits `target` vertically, placing `window` to its right.
    pub fn split_vertical(
        &mut self,
        target: WinHandle,
        window: WinHandle,
        state: WindowState,
    ) -> Result<(), LayoutError> {
        self.split(target, window, state, SplitAxis::Vertical)
    }

    /// Splits `target` horizontally, placing `window` below it.
    pub fn split_horizontal(
        &mut self,
        target: WinHandle,
        window: WinHandle,
        state: WindowState,
    ) -> Result<(), LayoutError> {
        self.split(target, window, state, SplitAxis::Horizontal)
    }

    /// Closes a tiled window and collapses redundant containers.
    ///
    /// When the current window closes, the window that takes its old preorder
    /// position becomes current, or the preceding window when it was last.
    pub fn close(&mut self, window: WinHandle) -> Result<WindowState, LayoutError> {
        let resolved = self.resolve(window);
        let old_order = self.windows();
        let old_index = old_order
            .iter()
            .position(|candidate| *candidate == resolved)
            .ok_or(LayoutError::UnknownWindow(resolved))?;
        if old_order.len() == 1 {
            return Err(LayoutError::LastWindow);
        }
        let replacement_current = old_order
            .iter()
            .skip(old_index + 1)
            .chain(old_order[..old_index].iter().rev())
            .next()
            .copied()
            .ok_or(LayoutError::LastWindow)?;

        let removed = remove_leaf(&mut self.root, resolved)
            .ok_or(LayoutError::UnknownWindow(resolved))?;
        collapse_containers(&mut self.root);
        let geometry = self.root.geometry();
        equalize_frame(&mut self.root, geometry)?;

        if self.current == resolved {
            self.current = replacement_current;
        }
        Ok(removed.state)
    }

    /// Changes the root rectangle and equalizes every split below it.
    pub fn resize(&mut self, geometry: Geometry) -> Result<(), LayoutError> {
        equalize_frame(&mut self.root, geometry)
    }

    /// Redistributes every split equally within the current root rectangle.
    pub fn equalize(&mut self) -> Result<(), LayoutError> {
        let geometry = self.root.geometry();
        equalize_frame(&mut self.root, geometry)
    }

    fn resolve(&self, window: WinHandle) -> WinHandle {
        if window.is_current() {
            self.current
        } else {
            window
        }
    }

    fn split(
        &mut self,
        target: WinHandle,
        window: WinHandle,
        state: WindowState,
        axis: SplitAxis,
    ) -> Result<(), LayoutError> {
        validate_identity(window)?;
        if find_leaf(&self.root, window).is_some() {
            return Err(LayoutError::DuplicateWindow(window));
        }
        let target = self.resolve(target);
        let target_geometry = find_leaf(&self.root, target)
            .map(|leaf| leaf.geometry)
            .ok_or(LayoutError::UnknownWindow(target))?;
        let available = match axis {
            SplitAxis::Vertical => target_geometry.width,
            SplitAxis::Horizontal => target_geometry.height,
        };
        if available < 2 {
            return Err(LayoutError::InsufficientSpace {
                available,
                children: 2,
            });
        }

        let new_leaf = Frame::Leaf(LeafFrame {
            window,
            state,
            geometry: target_geometry,
        });
        // Same-axis splits join the matching ancestor container instead of
        // nesting (upstream `win_split_ins`): a vertical split of a window
        // already inside a row of columns is inserted into that row, so
        // equalization divides every sibling on the same axis. When no
        // same-axis container exists, wrap the leaf in a fresh one.
        if let Err(new_leaf) = insert_into_matching_container(&mut self.root, target, new_leaf, axis) {
            let old_leaf = find_leaf_mut(&mut self.root, target)
                .ok_or(LayoutError::UnknownWindow(target))?
                .clone();
            let replacement = match axis {
                SplitAxis::Vertical => Frame::Row {
                    geometry: target_geometry,
                    children: vec![Frame::Leaf(old_leaf), new_leaf],
                },
                SplitAxis::Horizontal => Frame::Column {
                    geometry: target_geometry,
                    children: vec![Frame::Leaf(old_leaf), new_leaf],
                },
            };
            replace_leaf(&mut self.root, target, replacement);
        }
        let geometry = self.root.geometry();
        equalize_frame(&mut self.root, geometry)?;
        self.current = window;
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum SplitAxis {
    Vertical,
    Horizontal,
}

/// Reference coordinate space for a floating window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelativeTo {
    /// The entire editor grid.
    Editor,
    /// Another window's grid.
    Window(WinHandle),
    /// The current cursor position.
    Cursor,
}

/// Point of the floating window attached to its configured row and column.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Anchor {
    /// North-west corner.
    NorthWest,
    /// North-east corner.
    NorthEast,
    /// South-west corner.
    SouthWest,
    /// South-east corner.
    SouthEast,
}

/// Border presentation for a floating window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Border {
    /// No border.
    None,
    /// Single-line border.
    Single,
    /// Double-line border.
    Double,
    /// Rounded border.
    Rounded,
    /// Solid-cell border.
    Solid,
    /// Drop-shadow border.
    Shadow,
    /// Eight pieces ordered top-left, top, top-right, right, bottom-right,
    /// bottom, bottom-left, left.
    Custom([String; 8]),
}

/// Optional text shown in a floating window's border.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BorderText {
    /// Text rendered in the border.
    pub text: String,
    /// Alignment within the available border span.
    pub alignment: TextAlignment,
}

/// Alignment for border text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextAlignment {
    /// Align at the beginning of the border span.
    Left,
    /// Center within the border span.
    Center,
    /// Align at the end of the border span.
    Right,
}

/// Extra cells reserved around floating-window content.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Margins {
    /// Cells above the content.
    pub top: usize,
    /// Cells to the right of the content.
    pub right: usize,
    /// Cells below the content.
    pub bottom: usize,
    /// Cells to the left of the content.
    pub left: usize,
}

/// Configuration for a floating window.
#[derive(Clone, Debug, PartialEq)]
pub struct WinConfig {
    /// Coordinate space containing `row` and `col`.
    pub relative: RelativeTo,
    /// Window corner attached to the configured position.
    pub anchor: Anchor,
    /// Row within the relative coordinate space; fractional values are kept.
    pub row: f64,
    /// Column within the relative coordinate space; fractional values are kept.
    pub col: f64,
    /// Content width in cells.
    pub width: usize,
    /// Content height in cells.
    pub height: usize,
    /// Stacking priority; larger values appear above smaller values.
    pub zindex: u32,
    /// Border presentation.
    pub border: Border,
    /// Optional top-border text.
    pub title: Option<BorderText>,
    /// Optional bottom-border text.
    pub footer: Option<BorderText>,
    /// Extra space around the content.
    pub margins: Margins,
}

impl WinConfig {
    /// Creates a valid floating-window configuration with no decoration.
    pub fn new(
        relative: RelativeTo,
        anchor: Anchor,
        row: f64,
        col: f64,
        width: usize,
        height: usize,
    ) -> Result<Self, LayoutError> {
        if width == 0 || height == 0 {
            return Err(LayoutError::InvalidDimensions { width, height });
        }
        if !row.is_finite() || !col.is_finite() {
            return Err(LayoutError::InvalidCoordinate);
        }
        Ok(Self {
            relative,
            anchor,
            row,
            col,
            width,
            height,
            zindex: 50,
            border: Border::None,
            title: None,
            footer: None,
            margins: Margins::default(),
        })
    }

    /// Revalidates fields after direct configuration changes.
    pub fn validate(&self) -> Result<(), LayoutError> {
        if self.width == 0 || self.height == 0 {
            return Err(LayoutError::InvalidDimensions {
                width: self.width,
                height: self.height,
            });
        }
        if !self.row.is_finite() || !self.col.is_finite() {
            return Err(LayoutError::InvalidCoordinate);
        }
        Ok(())
    }
}

/// A floating window and its viewport state.
#[derive(Clone, Debug, PartialEq)]
pub struct FloatingWindow {
    /// Stable window identity.
    pub window: WinHandle,
    /// Buffer and viewport state.
    pub state: WindowState,
    /// Floating-window presentation.
    pub config: WinConfig,
}

/// Window state owned by one tabpage.
#[derive(Clone, Debug, PartialEq)]
pub struct TabpageState {
    layout: Layout,
    floats: Vec<FloatingWindow>,
    current: WinHandle,
}

impl TabpageState {
    /// Creates a tabpage around a tiled layout.
    #[must_use]
    pub fn new(layout: Layout) -> Self {
        let current = layout.current_window();
        Self {
            layout,
            floats: Vec::new(),
            current,
        }
    }

    /// Returns the tiled layout.
    #[must_use]
    pub const fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Returns the current tiled or floating window.
    #[must_use]
    pub const fn current_window(&self) -> WinHandle {
        self.current
    }

    /// Returns floating windows from lowest to highest z-index. Equal z-index
    /// windows remain in insertion order.
    pub fn floating_windows(&self) -> impl ExactSizeIterator<Item = &FloatingWindow> {
        self.floats.iter()
    }

    /// Returns state for a tiled or floating window.
    pub fn window(&self, window: WinHandle) -> Result<&WindowState, LayoutError> {
        let resolved = self.resolve(window);
        if let Some(float) = self
            .floats
            .iter()
            .find(|candidate| candidate.window == resolved)
        {
            return Ok(&float.state);
        }
        self.layout.window(resolved)
    }

    /// Returns mutable state for a tiled or floating window.
    pub fn window_mut(&mut self, window: WinHandle) -> Result<&mut WindowState, LayoutError> {
        let resolved = self.resolve(window);
        if let Some(index) = self
            .floats
            .iter()
            .position(|candidate| candidate.window == resolved)
        {
            return Ok(&mut self.floats[index].state);
        }
        self.layout.window_mut(resolved)
    }

    /// Makes a tiled or floating window current.
    pub fn set_current(&mut self, window: WinHandle) -> Result<(), LayoutError> {
        let resolved = self.resolve(window);
        if self
            .floats
            .iter()
            .any(|candidate| candidate.window == resolved)
        {
            self.current = resolved;
            return Ok(());
        }
        self.layout.set_current(resolved)?;
        self.current = resolved;
        Ok(())
    }

    /// Adds a floating window while preserving stable z-index ordering.
    pub fn add_float(
        &mut self,
        window: WinHandle,
        state: WindowState,
        config: WinConfig,
    ) -> Result<(), LayoutError> {
        validate_identity(window)?;
        config.validate()?;
        if self.contains(window) {
            return Err(LayoutError::DuplicateWindow(window));
        }
        let index = match self
            .floats
            .iter()
            .position(|candidate| candidate.config.zindex > config.zindex)
        {
            Some(index) => index,
            None => self.floats.len(),
        };
        self.floats.insert(
            index,
            FloatingWindow {
                window,
                state,
                config,
            },
        );
        Ok(())
    }

    /// Removes a floating window.
    pub fn remove_float(&mut self, window: WinHandle) -> Result<FloatingWindow, LayoutError> {
        let resolved = self.resolve(window);
        let index = self
            .floats
            .iter()
            .position(|candidate| candidate.window == resolved)
            .ok_or(LayoutError::UnknownWindow(resolved))?;
        let removed = self.floats.remove(index);
        if self.current == resolved {
            self.current = self.layout.current_window();
        }
        Ok(removed)
    }

    /// Closes a tiled window and keeps the tabpage current window valid.
    pub fn close_tiled(&mut self, window: WinHandle) -> Result<WindowState, LayoutError> {
        let resolved = self.resolve(window);
        let removed = self.layout.close(resolved)?;
        if self.current == resolved {
            self.current = self.layout.current_window();
        }
        Ok(removed)
    }

    /// Splits a tiled window vertically.
    pub fn split_vertical(
        &mut self,
        target: WinHandle,
        window: WinHandle,
        state: WindowState,
    ) -> Result<(), LayoutError> {
        validate_identity(window)?;
        if self.contains(window) {
            return Err(LayoutError::DuplicateWindow(window));
        }
        let target = self.resolve(target);
        self.layout.split_vertical(target, window, state)?;
        self.current = window;
        Ok(())
    }

    /// Splits a tiled window horizontally.
    pub fn split_horizontal(
        &mut self,
        target: WinHandle,
        window: WinHandle,
        state: WindowState,
    ) -> Result<(), LayoutError> {
        validate_identity(window)?;
        if self.contains(window) {
            return Err(LayoutError::DuplicateWindow(window));
        }
        let target = self.resolve(target);
        self.layout.split_horizontal(target, window, state)?;
        self.current = window;
        Ok(())
    }

    /// Resizes and equalizes the tiled layout.
    pub fn resize(&mut self, geometry: Geometry) -> Result<(), LayoutError> {
        self.layout.resize(geometry)
    }

    /// Equalizes the tiled layout within its current rectangle.
    pub fn equalize(&mut self) -> Result<(), LayoutError> {
        self.layout.equalize()
    }

    fn resolve(&self, window: WinHandle) -> WinHandle {
        if window.is_current() {
            self.current
        } else {
            window
        }
    }

    fn contains(&self, window: WinHandle) -> bool {
        self.floats
            .iter()
            .any(|candidate| candidate.window == window)
            || self.layout.window(window).is_ok()
    }
}

fn validate_identity(window: WinHandle) -> Result<(), LayoutError> {
    if window.is_current() {
        Err(LayoutError::CurrentWindow)
    } else {
        Ok(())
    }
}

fn validate_geometry(geometry: Geometry) -> Result<(), LayoutError> {
    if geometry.width == 0 || geometry.height == 0 {
        return Err(LayoutError::InvalidDimensions {
            width: geometry.width,
            height: geometry.height,
        });
    }
    if geometry.row.checked_add(geometry.height).is_none()
        || geometry.col.checked_add(geometry.width).is_none()
    {
        return Err(LayoutError::GeometryOverflow);
    }
    Ok(())
}

fn find_leaf(frame: &Frame, window: WinHandle) -> Option<&LeafFrame> {
    match frame {
        Frame::Leaf(leaf) => (leaf.window == window).then_some(leaf),
        Frame::Row { children, .. } | Frame::Column { children, .. } => {
            children.iter().find_map(|child| find_leaf(child, window))
        }
    }
}

fn find_leaf_mut(frame: &mut Frame, window: WinHandle) -> Option<&mut LeafFrame> {
    match frame {
        Frame::Leaf(leaf) => (leaf.window == window).then_some(leaf),
        Frame::Row { children, .. } | Frame::Column { children, .. } => children
            .iter_mut()
            .find_map(|child| find_leaf_mut(child, window)),
    }
}

/// Lists the child indices from the root down to (but not including) the leaf
/// whose window matches `wanted`.
fn collect_container_path(frame: &Frame, wanted: WinHandle, path: &mut Vec<usize>) -> bool {
    match frame {
        Frame::Leaf(leaf) => leaf.window == wanted,
        Frame::Row { children, .. } | Frame::Column { children, .. } => children
            .iter()
            .enumerate()
            .find_map(|(index, child)| {
                collect_container_path(child, wanted, path).then_some(index)
            })
            .is_some_and(|index| {
                path.push(index);
                true
            }),
    }
}

/// Returns the frame reached by descending `path` from `frame`.
fn container_at<'a>(frame: &'a Frame, path: &[usize]) -> &'a Frame {
    let mut current = frame;
    for &index in path {
        current = match current {
            Frame::Leaf(_) => unreachable!("split path descends only through containers"),
            Frame::Row { children, .. } | Frame::Column { children, .. } => &children[index],
        };
    }
    current
}

fn container_at_mut<'a>(frame: &'a mut Frame, path: &[usize]) -> &'a mut Frame {
    let mut current = frame;
    for &index in path {
        current = match current {
            Frame::Leaf(_) => unreachable!("split path descends only through containers"),
            Frame::Row { children, .. } | Frame::Column { children, .. } => &mut children[index],
        };
    }
    current
}

/// Whether `frame` stacks its children along `axis` (columns for a Row of
/// vertical splits, rows for a Column of horizontal splits).
fn matches_axis(frame: &Frame, axis: SplitAxis) -> bool {
    matches!(
        (axis, frame),
        (SplitAxis::Vertical, Frame::Row { .. }) | (SplitAxis::Horizontal, Frame::Column { .. })
    )
}

/// Inserts `new_leaf` as a sibling of `window` inside the target's immediate
/// parent container, but only when that parent already stacks children along
/// `axis`. If the immediate parent has the opposite orientation, or `window`
/// is the root leaf, returns `Err(new_leaf)` so the caller can wrap the
/// target in a fresh container instead (upstream `win_split_ins`).
fn insert_into_matching_container(
    root: &mut Frame,
    window: WinHandle,
    new_leaf: Frame,
    axis: SplitAxis,
) -> Result<(), Frame> {
    let mut path = Vec::new();
    if !collect_container_path(root, window, &mut path) {
        return Err(new_leaf);
    }
    if path.is_empty() {
        return Err(new_leaf);
    }
    let parent_path = &path[..path.len() - 1];
    if !matches_axis(container_at(root, parent_path), axis) {
        return Err(new_leaf);
    }
    let target_index = path[path.len() - 1];
    let insert_at = target_index.saturating_add(1);
    match container_at_mut(root, parent_path) {
        Frame::Leaf(_) => unreachable!("split path ends in a container"),
        Frame::Row { children, .. } | Frame::Column { children, .. } => {
            children.insert(insert_at.min(children.len()), new_leaf);
        }
    }
    Ok(())
}

fn replace_leaf(frame: &mut Frame, window: WinHandle, replacement: Frame) -> bool {
    fn replace(frame: &mut Frame, window: WinHandle, replacement: &mut Option<Frame>) -> bool {
        match frame {
            Frame::Leaf(leaf) if leaf.window == window => {
                if let Some(replacement) = replacement.take() {
                    *frame = replacement;
                    true
                } else {
                    false
                }
            }
            Frame::Leaf(_) => false,
            Frame::Row { children, .. } | Frame::Column { children, .. } => children
                .iter_mut()
                .any(|child| replace(child, window, replacement)),
        }
    }

    let mut replacement = Some(replacement);
    replace(frame, window, &mut replacement)
}

fn remove_leaf(frame: &mut Frame, window: WinHandle) -> Option<LeafFrame> {
    let children = match frame {
        Frame::Leaf(_) => return None,
        Frame::Row { children, .. } | Frame::Column { children, .. } => children,
    };

    if let Some(index) = children
        .iter()
        .position(|child| matches!(child, Frame::Leaf(leaf) if leaf.window == window))
    {
        return match children.remove(index) {
            Frame::Leaf(leaf) => Some(leaf),
            Frame::Row { .. } | Frame::Column { .. } => None,
        };
    }
    children
        .iter_mut()
        .find_map(|child| remove_leaf(child, window))
}

fn collapse_containers(frame: &mut Frame) {
    let replacement = match frame {
        Frame::Leaf(_) => None,
        Frame::Row { children, .. } | Frame::Column { children, .. } => {
            for child in children.iter_mut() {
                collapse_containers(child);
            }
            if children.len() == 1 {
                children.pop()
            } else {
                None
            }
        }
    };
    if let Some(child) = replacement {
        *frame = child;
        collapse_containers(frame);
    }
}

fn preorder_number(frame: &Frame, window: WinHandle, next: &mut usize) -> Option<usize> {
    match frame {
        Frame::Leaf(leaf) => {
            let number = *next;
            *next += 1;
            (leaf.window == window).then_some(number)
        }
        Frame::Row { children, .. } | Frame::Column { children, .. } => children
            .iter()
            .find_map(|child| preorder_number(child, window, next)),
    }
}

fn preorder_window(frame: &Frame, wanted: usize, next: &mut usize) -> Option<WinHandle> {
    match frame {
        Frame::Leaf(leaf) => {
            let number = *next;
            *next += 1;
            (number == wanted).then_some(leaf.window)
        }
        Frame::Row { children, .. } | Frame::Column { children, .. } => children
            .iter()
            .find_map(|child| preorder_window(child, wanted, next)),
    }
}

fn collect_windows(frame: &Frame, windows: &mut Vec<WinHandle>) {
    match frame {
        Frame::Leaf(leaf) => windows.push(leaf.window),
        Frame::Row { children, .. } | Frame::Column { children, .. } => {
            for child in children {
                collect_windows(child, windows);
            }
        }
    }
}

fn equalize_frame(frame: &mut Frame, geometry: Geometry) -> Result<(), LayoutError> {
    validate_frame_geometry(frame, geometry)?;
    assign_frame_geometry(frame, geometry);
    Ok(())
}

fn validate_frame_geometry(frame: &Frame, geometry: Geometry) -> Result<(), LayoutError> {
    validate_geometry(geometry)?;

    match frame {
        Frame::Leaf(_) => Ok(()),
        Frame::Row { children, .. } => {
            validate_children_geometry(children, geometry, SplitAxis::Vertical)
        }
        Frame::Column { children, .. } => {
            validate_children_geometry(children, geometry, SplitAxis::Horizontal)
        }
    }
}

fn validate_children_geometry(
    children: &[Frame],
    geometry: Geometry,
    axis: SplitAxis,
) -> Result<(), LayoutError> {
    let available = match axis {
        SplitAxis::Vertical => geometry.width,
        SplitAxis::Horizontal => geometry.height,
    };
    if children.is_empty() || available < children.len() {
        return Err(LayoutError::InsufficientSpace {
            available,
            children: children.len(),
        });
    }

    let base = available / children.len();
    let remainder = available % children.len();
    let mut offset = 0usize;
    for (index, child) in children.iter().enumerate() {
        let extent = base + usize::from(index < remainder);
        let child_geometry = match axis {
            SplitAxis::Vertical => Geometry {
                row: geometry.row,
                col: geometry
                    .col
                    .checked_add(offset)
                    .ok_or(LayoutError::GeometryOverflow)?,
                width: extent,
                height: geometry.height,
            },
            SplitAxis::Horizontal => Geometry {
                row: geometry
                    .row
                    .checked_add(offset)
                    .ok_or(LayoutError::GeometryOverflow)?,
                col: geometry.col,
                width: geometry.width,
                height: extent,
            },
        };
        validate_frame_geometry(child, child_geometry)?;
        offset = offset
            .checked_add(extent)
            .ok_or(LayoutError::GeometryOverflow)?;
    }
    Ok(())
}

fn assign_frame_geometry(frame: &mut Frame, geometry: Geometry) {
    match frame {
        Frame::Leaf(leaf) => leaf.geometry = geometry,
        Frame::Row {
            geometry: own,
            children,
        } => {
            *own = geometry;
            assign_children_geometry(children, geometry, SplitAxis::Vertical);
        }
        Frame::Column {
            geometry: own,
            children,
        } => {
            *own = geometry;
            assign_children_geometry(children, geometry, SplitAxis::Horizontal);
        }
    }
}

fn assign_children_geometry(children: &mut [Frame], geometry: Geometry, axis: SplitAxis) {
    let available = match axis {
        SplitAxis::Vertical => geometry.width,
        SplitAxis::Horizontal => geometry.height,
    };
    let base = available / children.len();
    let remainder = available % children.len();
    let mut offset = 0usize;
    for (index, child) in children.iter_mut().enumerate() {
        let extent = base + usize::from(index < remainder);
        let child_geometry = match axis {
            SplitAxis::Vertical => Geometry {
                row: geometry.row,
                col: geometry.col + offset,
                width: extent,
                height: geometry.height,
            },
            SplitAxis::Horizontal => Geometry {
                row: geometry.row + offset,
                col: geometry.col,
                width: geometry.width,
                height: extent,
            },
        };
        assign_frame_geometry(child, child_geometry);
        offset += extent;
    }
}
