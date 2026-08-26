//! Vim-compatible register storage.

use thiserror::Error;

/// The shape of text stored in a register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegisterKind {
    /// Text is inserted at a byte-column position.
    CharacterWise,
    /// Complete logical lines are inserted after the target line.
    LineWise,
    /// A rectangle whose rows are padded before insertion.
    BlockWise {
        /// Rectangle width in bytes.
        width: usize,
    },
}

/// Validated text held by a register.
///
/// Lines never contain line separators and always contain valid UTF-8. A
/// characterwise value may have several lines; their separators are restored
/// when the value is rendered as bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisterContent {
    kind: RegisterKind,
    lines: Vec<Vec<u8>>,
}

impl RegisterContent {
    /// Creates validated register content from newline-free logical lines.
    pub fn new(kind: RegisterKind, lines: Vec<Vec<u8>>) -> Result<Self, RegisterError> {
        let lines = if lines.is_empty() { vec![Vec::new()] } else { lines };
        for line in &lines {
            validate_register_line(line)?;
        }
        if let RegisterKind::BlockWise { width } = kind {
            if width == 0 || lines.iter().any(|line| line.len() > width) {
                return Err(RegisterError::InvalidBlockWidth { width });
            }
        }
        Ok(Self { kind, lines })
    }

    /// Creates a characterwise value from serialized UTF-8 text.
    pub fn characterwise(bytes: &[u8]) -> Result<Self, RegisterError> {
        let text = std::str::from_utf8(bytes).map_err(|_| RegisterError::InvalidUtf8)?;
        let lines = text.split('\n').map(|line| line.as_bytes().to_vec()).collect();
        Self::new(RegisterKind::CharacterWise, lines)
    }

    /// Creates a linewise value from newline-free logical lines.
    pub fn linewise(lines: Vec<Vec<u8>>) -> Result<Self, RegisterError> {
        Self::new(RegisterKind::LineWise, lines)
    }

    /// Creates a rectangular value with an exact byte width.
    pub fn blockwise(lines: Vec<Vec<u8>>, width: usize) -> Result<Self, RegisterError> {
        Self::new(RegisterKind::BlockWise { width }, lines)
    }

    /// Returns the register's text shape.
    #[must_use]
    pub const fn kind(&self) -> RegisterKind {
        self.kind
    }

    /// Returns the newline-free logical rows.
    #[must_use]
    pub fn lines(&self) -> &[Vec<u8>] {
        &self.lines
    }

    /// Serializes the rows with line-feed separators.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let separators = self.lines.len().saturating_sub(1);
        let capacity = self
            .lines
            .iter()
            .fold(separators, |total, line| total.saturating_add(line.len()));
        let mut bytes = Vec::with_capacity(capacity);
        for (index, line) in self.lines.iter().enumerate() {
            if index != 0 {
                bytes.push(b'\n');
            }
            bytes.extend_from_slice(line);
        }
        bytes
    }

    fn append(&mut self, other: &Self) {
        if matches!(other.kind, RegisterKind::LineWise) {
            self.kind = RegisterKind::LineWise;
            self.lines.extend(other.lines.iter().cloned());
            return;
        }

        if matches!(self.kind, RegisterKind::CharacterWise) {
            let Some(first) = other.lines.first() else {
                return;
            };
            let Some(last) = self.lines.last_mut() else {
                self.lines = other.lines.clone();
                return;
            };
            last.extend_from_slice(first);
            self.lines.extend(other.lines.iter().skip(1).cloned());
            return;
        }

        if let RegisterKind::BlockWise { width } = self.kind {
            let incoming_width = match other.kind {
                RegisterKind::BlockWise { width } => width,
                RegisterKind::CharacterWise | RegisterKind::LineWise => other
                    .lines
                    .iter()
                    .fold(0, |maximum, line| maximum.max(line.len())),
            };
            self.kind = RegisterKind::BlockWise {
                width: width.max(incoming_width),
            };
        }
        self.lines.extend(other.lines.iter().cloned());
    }
}

/// One of the two selection registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    /// The primary selection (`*`).
    Primary,
    /// The clipboard selection (`+`).
    Clipboard,
}

/// Host integration for the `*` and `+` registers.
///
/// The defaults deliberately expose no clipboard. Hosts may implement only the
/// operations they support.
pub trait ClipboardProvider {
    /// Reads a selection, or returns `None` when no provider/data is available.
    fn get(&mut self, _selection: Selection) -> Result<Option<RegisterContent>, RegisterError> {
        Ok(None)
    }

    /// Writes a selection. The default is an unavailable-provider no-op.
    fn set(
        &mut self,
        _selection: Selection,
        _content: &RegisterContent,
    ) -> Result<(), RegisterError> {
        Ok(())
    }
}



/// Failures from register parsing, integration, or buffer insertion.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum RegisterError {
    /// A register name is not part of the supported Vim register set.
    #[error("invalid register name {0:?}")]
    InvalidName(char),
    /// Register text was not valid UTF-8.
    #[error("register text must be valid UTF-8")]
    InvalidUtf8,
    /// A logical register line contained a line separator.
    #[error("a register line must not contain a newline")]
    NewlineInLine,
    /// The declared rectangle cannot contain all rows.
    #[error("invalid blockwise register width {width}")]
    InvalidBlockWidth {
        /// Declared rectangle width in bytes.
        width: usize,
    },
    /// The requested byte column lies beyond a line where padding is invalid.
    #[error("byte column {col} is outside line {lnum}, whose length is {line_len}")]
    ColumnOutOfBounds {
        /// One-based target line.
        lnum: usize,
        /// Zero-based target byte column.
        col: usize,
        /// Target line length in bytes.
        line_len: usize,
    },
    /// The requested byte column splits a UTF-8 code point.
    #[error("byte column {col} on line {lnum} is not a UTF-8 boundary")]
    NotCharBoundary {
        /// One-based target line.
        lnum: usize,
        /// Zero-based target byte column.
        col: usize,
    },
    /// Computing a blockwise target line overflowed.
    #[error("target line number overflow")]
    PositionOverflow,
    /// A read-only or externally-owned register was written without its host seam.
    #[error("register {0:?} requires a host provider")]
    ProviderRequired(char),
    /// The clipboard provider rejected an operation.
    #[error("clipboard provider failed: {0}")]
    Clipboard(String),
    /// The expression evaluator rejected an operation.
    #[error("expression evaluator failed: {0}")]
    Expression(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegisterName {
    Named { index: usize, append: bool },
    Numbered(usize),
    Unnamed,
    SmallDelete,
    BlackHole,
    Expression,
    Selection(Selection),
}

impl TryFrom<char> for RegisterName {
    type Error = RegisterError;

    fn try_from(name: char) -> Result<Self, Self::Error> {
        match name {
            'a'..='z' => Ok(Self::Named {
                index: usize::from(name as u8 - b'a'),
                append: false,
            }),
            'A'..='Z' => Ok(Self::Named {
                index: usize::from(name as u8 - b'A'),
                append: true,
            }),
            '0'..='9' => Ok(Self::Numbered(usize::from(name as u8 - b'0'))),
            '"' => Ok(Self::Unnamed),
            '-' => Ok(Self::SmallDelete),
            '_' => Ok(Self::BlackHole),
            '=' => Ok(Self::Expression),
            '*' => Ok(Self::Selection(Selection::Primary)),
            '+' => Ok(Self::Selection(Selection::Clipboard)),
            _ => Err(RegisterError::InvalidName(name)),
        }
    }
}

/// The editor's local register bank.
///
/// Clipboard and expression values are resolved through host traits rather
/// than retained here. All mutation is single-writer `&mut self` state.
#[derive(Clone, Debug)]
pub struct Registers {
    named: [Option<RegisterContent>; 26],
    numbered: [Option<RegisterContent>; 10],
    unnamed: Option<RegisterContent>,
    small_delete: Option<RegisterContent>,
    expression_source: Option<Vec<u8>>,
}

impl Default for Registers {
    fn default() -> Self {
        Self::new()
    }
}

impl Registers {
    /// Creates an empty register bank.
    #[must_use]
    pub fn new() -> Self {
        Self {
            named: std::array::from_fn(|_| None),
            numbered: std::array::from_fn(|_| None),
            unnamed: None,
            small_delete: None,
            expression_source: None,
        }
    }

    /// Returns stored content. Provider-backed registers return `None`.
    pub fn get(&self, name: char) -> Result<Option<&RegisterContent>, RegisterError> {
        match RegisterName::try_from(name)? {
            RegisterName::Named { index, .. } => Ok(self.named.get(index).and_then(Option::as_ref)),
            RegisterName::Numbered(index) => {
                Ok(self.numbered.get(index).and_then(Option::as_ref))
            }
            RegisterName::Unnamed => Ok(self.unnamed.as_ref()),
            RegisterName::SmallDelete => Ok(self.small_delete.as_ref()),
            RegisterName::BlackHole
            | RegisterName::Expression
            | RegisterName::Selection(_) => Ok(None),
        }
    }

    /// Stores content in a writable register.
    ///
    /// Uppercase names append to their lowercase register. Selection registers
    /// require [`Self::set_with_clipboard`]. The black-hole register discards
    /// the value successfully.
    pub fn set(&mut self, name: char, content: RegisterContent) -> Result<(), RegisterError> {
        match RegisterName::try_from(name)? {
            RegisterName::Named { index, append } => {
                let Some(slot) = self.named.get_mut(index) else {
                    return Err(RegisterError::InvalidName(name));
                };
                write_slot(slot, content, append)
            }
            RegisterName::Numbered(index) => {
                let Some(slot) = self.numbered.get_mut(index) else {
                    return Err(RegisterError::InvalidName(name));
                };
                *slot = Some(content);
                Ok(())
            }
            RegisterName::Unnamed => {
                self.unnamed = Some(content);
                Ok(())
            }
            RegisterName::SmallDelete => {
                self.small_delete = Some(content);
                Ok(())
            }
            RegisterName::BlackHole => Ok(()),
            RegisterName::Expression => {
                self.expression_source = Some(content.to_bytes());
                Ok(())
            }
            RegisterName::Selection(_) => Err(RegisterError::ProviderRequired(name)),
        }
    }

    /// Stores content, forwarding selection registers to the clipboard host.
    pub fn set_with_clipboard(
        &mut self,
        name: char,
        content: RegisterContent,
        clipboard: &mut dyn ClipboardProvider,
    ) -> Result<(), RegisterError> {
        match RegisterName::try_from(name)? {
            RegisterName::Selection(selection) => clipboard.set(selection, &content),
            _ => self.set(name, content),
        }
    }

    /// Records an ordinary yank in register `0` and the unnamed register.
    pub fn yank(&mut self, content: RegisterContent) {
        if let Some(slot) = self.numbered.get_mut(0) {
            *slot = Some(content.clone());
        }
        self.unnamed = Some(content);
    }

    /// Records a yank in an explicit register and updates the unnamed register.
    ///
    /// The black-hole register discards the yank and leaves unnamed unchanged.
    pub fn yank_to(
        &mut self,
        name: char,
        content: RegisterContent,
    ) -> Result<(), RegisterError> {
        if RegisterName::try_from(name)? == RegisterName::BlackHole {
            return Ok(());
        }
        self.set(name, content.clone())?;
        self.unnamed = self.get(name)?.cloned().or(Some(content));
        Ok(())
    }

    /// Records a yank, forwarding selection registers to the clipboard host.
    pub fn yank_to_with_clipboard(
        &mut self,
        name: char,
        content: RegisterContent,
        clipboard: &mut dyn ClipboardProvider,
    ) -> Result<(), RegisterError> {
        match RegisterName::try_from(name)? {
            RegisterName::Selection(selection) => {
                clipboard.set(selection, &content)?;
                self.unnamed = Some(content);
                Ok(())
            }
            _ => self.yank_to(name, content),
        }
    }

    /// Records a delete using Vim's small-delete and numbered rotation rules.
    ///
    /// A one-row characterwise deletion uses `-`; every other deletion shifts
    /// registers `1` through `9`. Both paths also update the unnamed register.
    pub fn delete(&mut self, content: RegisterContent) {
        let small = content.kind == RegisterKind::CharacterWise && content.lines.len() == 1;
        if small {
            self.small_delete = Some(content.clone());
        } else {
            for destination in (2..=9).rev() {
                let source = destination - 1;
                let shifted = self.numbered.get(source).cloned().flatten();
                if let Some(slot) = self.numbered.get_mut(destination) {
                    *slot = shifted;
                }
            }
            if let Some(slot) = self.numbered.get_mut(1) {
                *slot = Some(content.clone());
            }
        }
        self.unnamed = Some(content);
    }

    /// Records a delete in an explicit register and updates unnamed.
    pub fn delete_to(
        &mut self,
        name: char,
        content: RegisterContent,
    ) -> Result<(), RegisterError> {
        self.yank_to(name, content)
    }
}

fn validate_register_line(line: &[u8]) -> Result<(), RegisterError> {
    if line.contains(&b'\n') {
        return Err(RegisterError::NewlineInLine);
    }
    std::str::from_utf8(line)
        .map(|_| ())
        .map_err(|_| RegisterError::InvalidUtf8)
}

fn write_slot(
    slot: &mut Option<RegisterContent>,
    content: RegisterContent,
    append: bool,
) -> Result<(), RegisterError> {
    if append {
        if let Some(existing) = slot {
            existing.append(&content);
            return Ok(());
        }
    }
    *slot = Some(content);
    Ok(())
}
