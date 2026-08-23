//! Generated Ex command metadata and command-name resolution.

/// Bit flags copied from Neovim's `ex_cmds.lua` table.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CommandFlags(pub(crate) u32);

impl CommandFlags {
    /// Command accepts a line or domain-specific range.
    pub const RANGE: Self = Self(0x001);
    /// Command accepts a trailing bang.
    pub const BANG: Self = Self(0x002);
    /// Command accepts arguments.
    pub const EXTRA: Self = Self(0x004);
    /// Command defaults to the whole buffer when no range is given.
    pub const DFLALL: Self = Self(0x020);
    /// Command requires an argument.
    pub const NEEDARG: Self = Self(0x080);
    /// A bar may terminate this command.
    pub const TRLBAR: Self = Self(0x100);
    /// Command accepts a register argument.
    pub const REGSTR: Self = Self(0x200);
    /// Command accepts a count after its name.
    pub const COUNT: Self = Self(0x400);
    /// A double quote in the argument is not a trailing comment.
    pub const NOTRLCOM: Self = Self(0x800);

    /// Returns whether all bits in `other` are present.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Returns the raw upstream bit mask.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }
}

/// The domain an Ex command's addresses count in.
///
/// Copied from `addr_type` in Neovim's `ex_cmds.lua`. Address validation
/// depends on it: `invalid_range` (`ex_docmd.c:3735-3820`) bounds each domain
/// against a different limit, and [`AddrType::Other`] accepts any range.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum AddrType {
    /// Buffer line numbers, bounded by the line count.
    Lines,
    /// Window numbers in the current tabpage.
    Windows,
    /// Argument-list indices.
    Arguments,
    /// Buffer numbers.
    Buffers,
    /// Loaded buffer numbers.
    LoadedBuffers,
    /// Tabpage numbers.
    Tabs,
    /// Tabpage numbers relative to the current one.
    TabsRelative,
    /// Quickfix list indices.
    QuickFix,
    /// Quickfix list indices restricted to valid entries.
    QuickFixValid,
    /// A non-negative count with no domain limit.
    Unsigned,
    /// A domain-free number; any range is accepted.
    Other,
    /// The command takes no address at all.
    #[default]
    None,
}

/// Static metadata for one built-in Ex command.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CommandSpec {
    /// Full canonical command name.
    pub name: &'static str,
    /// Shortest prefix selecting this entry with upstream table ordering.
    pub abbr: &'static str,
    /// Byte length of `abbr`.
    pub min_prefix_len: usize,
    /// Upstream argument flags.
    pub flags: CommandFlags,
    /// Domain the command's addresses are counted in.
    pub addr_type: AddrType,
}

include!(concat!(env!("OUT_DIR"), "/command_specs.rs"));

/// Result supplied by a host's user-command registry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UserCommandMatch {
    /// No user command matches.
    None,
    /// One user command matches, with its canonical name.
    Match(String),
    /// More than one user command matches the typed prefix.
    Ambiguous,
}

/// Host seam for resolving commands created with `:command`.
pub trait UserCommandProvider {
    /// Resolves an uppercase user-command name or prefix.
    fn resolve_user_command(&self, typed: &str) -> UserCommandMatch;
}

/// Registry with no user-defined commands.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoUserCommands;

impl UserCommandProvider for NoUserCommands {
    fn resolve_user_command(&self, _typed: &str) -> UserCommandMatch {
        UserCommandMatch::None
    }
}

/// A resolved command name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolvedCommand {
    /// A generated built-in table entry.
    Builtin(&'static CommandSpec),
    /// A host-provided user command.
    User(String),
}

impl ResolvedCommand {
    /// Returns the canonical command name.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Builtin(spec) => spec.name,
            Self::User(name) => name,
        }
    }

    /// Returns built-in flags, or an empty mask for a user command.
    #[must_use]
    pub const fn flags(&self) -> CommandFlags {
        match self {
            Self::Builtin(spec) => spec.flags,
            Self::User(_) => CommandFlags(0),
        }
    }
}

/// Command lookup failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolveError {
    /// No built-in or user command matched.
    NotFound,
    /// The user-command provider reported an ambiguous prefix.
    AmbiguousUserCommand,
}

/// Resolves a command using Neovim's table-order rule.
///
/// Built-ins win whenever their prefix matches. User commands are considered
/// only as the uppercase-name fallback used by `find_ex_command()`.
pub fn resolve_command<P: UserCommandProvider + ?Sized>(
    typed: &str,
    users: &P,
) -> Result<ResolvedCommand, ResolveError> {
    if typed.is_empty() || typed == "ho" || typed == "def" {
        return Err(ResolveError::NotFound);
    }

    if typed == "s" {
        if let Some(spec) = command_spec("substitute") {
            return Ok(ResolvedCommand::Builtin(spec));
        }
    }
    if typed == "k" {
        if let Some(spec) = command_spec("k") {
            return Ok(ResolvedCommand::Builtin(spec));
        }
    }

    if let Some(spec) = COMMANDS.iter().find(|spec| spec.name.starts_with(typed)) {
        return Ok(ResolvedCommand::Builtin(spec));
    }

    if typed.as_bytes().first().is_some_and(u8::is_ascii_uppercase) {
        return match users.resolve_user_command(typed) {
            UserCommandMatch::None => Err(ResolveError::NotFound),
            UserCommandMatch::Match(name) => Ok(ResolvedCommand::User(name)),
            UserCommandMatch::Ambiguous => Err(ResolveError::AmbiguousUserCommand),
        };
    }
    Err(ResolveError::NotFound)
}

/// Finds a built-in by its exact canonical name.
#[must_use]
pub fn command_spec(name: &str) -> Option<&'static CommandSpec> {
    COMMANDS.iter().find(|spec| spec.name == name)
}
