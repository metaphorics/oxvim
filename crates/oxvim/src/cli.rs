//! Command-line parsing for the `oxvim` binary.
//!
//! The scanner mirrors `main.c` `command_line_scan`: a byte cursor walks each
//! `-` argument so clustered short options (`-Rn`) work, long options match
//! case-insensitively by prefix, and options taking a separate argument reject
//! trailing garbage before consuming the next word.

use std::fmt;

/// Upstream `MAX_ARG_CMDS`: the shared ceiling on `+cmd`, `-c` and `--cmd`.
const MAX_ARG_CMDS: usize = 10;

/// Selection for the user initialization file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UserConfig {
    /// Use normal startup discovery.
    Default,
    /// Skip all initialization files (`-u NONE`).
    None,
    /// Skip user initialization while retaining system defaults (`-u NORC`).
    NoRc,
    /// Source this exact file.
    File(String),
}

/// Selection for the ShaDa file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShadaConfig {
    /// Use the default ShaDa path.
    Default,
    /// Disable ShaDa (`-i NONE`).
    None,
    /// Use this exact file.
    File(String),
}

/// Non-interactive Ex input mode selected by `-e`, `-E`, `-es` or `-Es`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BatchMode {
    /// Suppress the interactive Ex prompt and message stream (`-es`, `-Es`,
    /// `-e -`); upstream `silent_mode`.
    pub silent: bool,
    /// Read standard input as buffer text rather than as Ex commands
    /// (`-E`, `-Es`); upstream `input_istext`.
    pub input_is_text: bool,
}

/// How the startup files are distributed over windows and tab pages.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WindowLayout {
    /// One window showing the first file (`main.c` `WIN_NONE`).
    #[default]
    Single,
    /// Horizontal splits (`-o`, `WIN_HOR`).
    Horizontal,
    /// Vertical splits (`-O`, `WIN_VER`).
    Vertical,
    /// Tab pages (`-p`, `WIN_TABS`).
    Tabs,
}

/// A Lua script and the arguments following it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LuaScript {
    /// Script filename.
    pub path: String,
    /// Arguments exposed as `arg[1]` onward.
    pub args: Vec<String>,
}

/// Verbosity configuration requested with `-V`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerboseConfig {
    /// Numeric verbosity level.
    pub level: u32,
    /// Optional log file suffix.
    pub file: Option<String>,
}

/// Parsed process arguments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cli {
    /// User initialization selection.
    pub user_config: UserConfig,
    /// ShaDa selection.
    pub shada: ShadaConfig,
    /// Factory-default startup.
    pub clean: bool,
    /// Use stdin/stdout as an RPC channel.
    pub embed: bool,
    /// Do not start a user interface.
    pub headless: bool,
    /// Listen for RPC at this address.
    pub listen: Option<String>,
    /// Execute a Lua file instead of entering the editor.
    pub lua_script: Option<LuaScript>,
    /// Read Ex input from stdin.
    pub batch: Option<BatchMode>,
    /// Read Normal-mode commands from this file (`-` means stdin).
    pub scriptin: Option<String>,
    /// Commands executed before configuration.
    pub pre_commands: Vec<String>,
    /// Commands executed after configuration.
    pub commands: Vec<String>,
    /// Verbosity level and optional log file requested with `-V`.
    pub verbose: Option<VerboseConfig>,
    /// Print API metadata and exit.
    pub api_info: bool,
    /// Print the help message and exit successfully (`-h`, `-?`, `--help`).
    pub help: bool,
    /// Print version information and exit successfully (`-v`, `--version`).
    pub version: bool,
    /// Open the first buffer read-only (`-R`).
    pub readonly: bool,
    /// Forbid writing files (`-m`, `-M`).
    pub no_write: bool,
    /// Reset `'modifiable'` (`-M`).
    pub no_modifiable: bool,
    /// Use memory only, without a swap file (`-n`, `-l`, `-e -`).
    pub no_swap_file: bool,
    /// Edit in binary mode (`-b`).
    pub binary: bool,
    /// Edit standard input as the sole file (`nvim -`).
    pub stdin_file: bool,
    /// Explicit `-o{N}`/`-O{N}`/`-p{N}` count; zero means "one per file".
    pub window_count: usize,
    /// Distribution of startup files over windows and tab pages.
    pub window_layout: WindowLayout,
    /// `'window'` value requested with `-w{number}`.
    pub window_height: Option<i64>,
    /// Write startup timing messages to this file (`--startuptime`).
    pub startuptime: Option<String>,
    /// Whether plugin scripts should be loaded on startup.
    ///
    /// `--noplugin` resets this to `false`; `-u NONE` also resets it unless
    /// `--clean` was given, matching upstream `main.c`.
    pub loadplugins: bool,
    /// Files to edit.
    pub files: Vec<String>,
}

impl Default for Cli {
    fn default() -> Self {
        Self {
            user_config: UserConfig::Default,
            shada: ShadaConfig::Default,
            clean: false,
            embed: false,
            headless: false,
            listen: None,
            lua_script: None,
            batch: None,
            scriptin: None,
            pre_commands: Vec::new(),
            commands: Vec::new(),
            verbose: None,
            api_info: false,
            help: false,
            version: false,
            readonly: false,
            no_write: false,
            no_modifiable: false,
            no_swap_file: false,
            binary: false,
            stdin_file: false,
            window_count: 0,
            window_layout: WindowLayout::Single,
            window_height: None,
            startuptime: None,
            loadplugins: true,
            files: Vec::new(),
        }
    }
}

/// A command-line usage failure.
///
/// `main.c` reports two shapes: `mainerr` prints `{program}: {message}` plus a
/// `-h` pointer and exits 1, while the duplicate-script error prints a bare
/// line and exits 2. Both are observable, so the exit status travels with the
/// message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageError {
    message: String,
    exit_code: u8,
    hint: bool,
}

impl UsageError {
    /// Upstream `mainerr`: `{program}: {msg}` with a `-h` pointer, exit 1.
    fn main_error(message: impl Into<String>) -> Self {
        Self { message: message.into(), exit_code: 1, hint: true }
    }

    /// Upstream `mainerr(msg, argument, NULL)`, which quotes the argument.
    fn about(message: &str, argument: &str) -> Self {
        Self::main_error(format!("{message}: \"{argument}\""))
    }

    /// Upstream `scripterror`: a bare message on stderr, exit 2.
    fn script_error(message: impl Into<String>) -> Self {
        Self { message: message.into(), exit_code: 2, hint: false }
    }

    /// The process status this failure exits with.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        self.exit_code
    }
}

/// The invoked program name, `main.c` `path_tail(argv0)`.
fn program_name() -> String {
    std::env::args_os()
        .next()
        .and_then(|argv0| {
            std::path::Path::new(&argv0)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "oxvim".to_owned())
}

impl fmt::Display for UsageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.hint {
            return write!(formatter, "{}", self.message);
        }
        let program = program_name();
        write!(formatter, "{program}: {}\nMore info with \"{program} -h\"", self.message)
    }
}

impl std::error::Error for UsageError {}

/// What a recognized option letter still needs before it can act.
enum Want {
    /// The option is complete; keep scanning this argument.
    Done,
    /// The rest of this argument was consumed; move to the next one.
    Argument,
    /// The option takes the next whole argument.
    Next(char),
}

impl Cli {
    /// Parse arguments excluding argv[0].
    pub fn parse<I, S>(arguments: I) -> Result<Self, UsageError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let args = arguments.into_iter().map(Into::into).collect::<Vec<_>>();
        let mut cli = Self::default();
        let mut index = 0;
        let mut had_minmin = false;

        while index < args.len() {
            let argument = args[index].clone();
            if !had_minmin && argument.starts_with('+') {
                let command =
                    if argument.len() == 1 { "$".to_owned() } else { argument[1..].to_owned() };
                cli.push_command(command)?;
                index += 1;
                continue;
            }
            if had_minmin || !argument.starts_with('-') {
                cli.files.push(argument);
                index += 1;
                continue;
            }

            // Walk the option cluster: `-Rn` is `-R` followed by `-n`.
            let mut cursor = 1;
            loop {
                let letter = argument[cursor..].chars().next();
                cursor += letter.map_or(0, char::len_utf8);
                let want = cli.scan_option(
                    letter,
                    &argument,
                    &mut cursor,
                    &mut had_minmin,
                )?;
                // main.c prints and exits inside the scan, so a later
                // unknown option is never reached.
                if cli.help || cli.version || cli.api_info {
                    return Ok(cli);
                }
                match want {
                    Want::Done if cursor < argument.len() => continue,
                    Want::Done | Want::Argument => break,
                    Want::Next(letter) => {
                        // main.c rejects garbage between the option letter and
                        // the end of the argument before taking the next word.
                        if cursor < argument.len() {
                            return Err(UsageError::about(
                                "Garbage after option argument",
                                &argument,
                            ));
                        }
                        // `-S` alone falls back to the default session file.
                        let value = args.get(index + 1);
                        if value.is_none() && letter != 'S' {
                            return Err(UsageError::about("Argument missing after", &argument));
                        }
                        if cli.take_argument(letter, &argument, value, &args, index + 1)? {
                            index += 1;
                        }
                        if letter == 'l' {
                            // "-l {file}" swallows every remaining argument.
                            index = args.len();
                        }
                        break;
                    }
                }
            }
            index += 1;
        }

        // main.c: `-u NONE` resets 'loadplugins' to false, unless `--clean`
        // was also given (`p_lpl = vimrc_none ? params.clean : p_lpl`).
        if cli.user_config == UserConfig::None {
            cli.loadplugins = cli.clean;
        }
        // main.c: `if (embedded_mode && (silent_mode || parmp->luaf))`.
        if cli.embed
            && (cli.batch.is_some_and(|batch| batch.silent) || cli.lua_script.is_some())
        {
            return Err(UsageError::main_error("--embed conflicts with -es/-Es/-l"));
        }
        Ok(cli)
    }

    /// Handle one option letter, or the long option that follows `--`.
    ///
    /// `cursor` already points past `letter` and is advanced over any inline
    /// value the option consumes.
    fn scan_option(
        &mut self,
        letter: Option<char>,
        argument: &str,
        cursor: &mut usize,
        had_minmin: &mut bool,
    ) -> Result<Want, UsageError> {
        let Some(letter) = letter else {
            // "nvim -": silent mode inside Ex mode, otherwise edit stdin.
            if let Some(batch) = self.batch.as_mut() {
                batch.silent = true;
                self.no_swap_file = true;
            } else {
                self.stdin_file = true;
            }
            return Ok(Want::Argument);
        };
        match letter {
            '-' => self.scan_long_option(argument, cursor, had_minmin),
            'A' => Err(unsupported(argument, "the 'arabic' option side effects and keymap files")),
            'b' => {
                self.binary = true;
                Ok(Want::Done)
            }
            'D' => Err(unsupported(argument, "the Ex debugger")),
            'd' => Err(unsupported(argument, "a diff engine")),
            'e' => {
                self.batch.get_or_insert_with(BatchMode::default);
                Ok(Want::Done)
            }
            'E' => {
                let batch = self.batch.get_or_insert_with(BatchMode::default);
                batch.input_is_text = true;
                Ok(Want::Done)
            }
            'f' | 'N' | 'X' => Ok(Want::Done),
            'h' | '?' => {
                self.help = true;
                Ok(Want::Done)
            }
            'H' => Err(unsupported(argument, "keymap file loading")),
            'M' => {
                self.no_modifiable = true;
                self.no_write = true;
                Ok(Want::Done)
            }
            'm' => {
                self.no_write = true;
                Ok(Want::Done)
            }
            'n' => {
                self.no_swap_file = true;
                Ok(Want::Done)
            }
            'p' | 'o' | 'O' => {
                self.window_count = number_argument(argument, cursor, 0);
                self.window_layout = match letter {
                    'p' => WindowLayout::Tabs,
                    'o' => WindowLayout::Horizontal,
                    _ => WindowLayout::Vertical,
                };
                Ok(Want::Done)
            }
            'q' => Err(unsupported(argument, "the quickfix list and 'errorformat'")),
            'R' => {
                self.readonly = true;
                Ok(Want::Done)
            }
            'r' | 'L' => Err(unsupported(argument, "swap-file recovery")),
            's' => {
                if let Some(batch) = self.batch.as_mut() {
                    // "-es"/"-Es": the silent batch modifier, not a script.
                    batch.silent = true;
                    return Ok(Want::Done);
                }
                Ok(Want::Next('s'))
            }
            't' => Err(unsupported(argument, "the tags subsystem")),
            'v' => {
                self.version = true;
                Ok(Want::Done)
            }
            'V' => {
                let level = number_argument(argument, cursor, 10);
                let file = &argument[*cursor..];
                *cursor = argument.len();
                self.verbose = Some(VerboseConfig {
                    level: u32::try_from(level).unwrap_or(u32::MAX),
                    file: (!file.is_empty()).then(|| file.to_owned()),
                });
                Ok(Want::Done)
            }
            'w' => {
                // "-w{number}" sets 'window'; "-w {scriptout}" records keys.
                if argument[*cursor..].starts_with(|c: char| c.is_ascii_digit()) {
                    let number = number_argument(argument, cursor, 10);
                    self.window_height = Some(i64::try_from(number).unwrap_or(i64::MAX));
                    return Ok(Want::Done);
                }
                Ok(Want::Next('w'))
            }
            'c' => {
                // "-c{command}" runs inline; "-c {command}" takes the next word.
                if *cursor < argument.len() {
                    self.push_command(argument[*cursor..].to_owned())?;
                    return Ok(Want::Argument);
                }
                Ok(Want::Next('c'))
            }
            'S' | 'i' | 'l' | 'u' | 'U' | 'W' => Ok(Want::Next(letter)),
            _ => Err(UsageError::about("Unknown option argument", argument)),
        }
    }

    /// Handle the long option in `argument`, whose name starts at `cursor`.
    ///
    /// Upstream compares case-insensitively, and every name except `help`,
    /// `version` and `api-info` matches by prefix, so `--noplugins` and
    /// `--literalxyz` are both accepted.
    fn scan_long_option(
        &mut self,
        argument: &str,
        cursor: &mut usize,
        had_minmin: &mut bool,
    ) -> Result<Want, UsageError> {
        let name = &argument[*cursor..];
        let exact = |expected: &str| name.eq_ignore_ascii_case(expected);
        let prefix = |expected: &str| {
            name.len() >= expected.len() && name[..expected.len()].eq_ignore_ascii_case(expected)
        };
        if exact("help") {
            self.help = true;
        } else if exact("version") {
            self.version = true;
        } else if exact("api-info") {
            self.api_info = true;
        } else if exact("headless") {
            self.headless = true;
        } else if exact("embed") {
            self.embed = true;
        } else if prefix("listen") {
            *cursor += "listen".len();
            return Ok(Want::Next('-'));
        } else if prefix("literal") {
            // Upstream no-op: file arguments are always literal (#7679).
        } else if prefix("remote") {
            return Err(unsupported(argument, "RPC client channels and vim._cs_remote"));
        } else if prefix("server") {
            return Err(unsupported(argument, "RPC client channels and vim._cs_remote"));
        } else if prefix("noplugin") {
            self.loadplugins = false;
        } else if prefix("cmd") {
            *cursor += "cmd".len();
            return Ok(Want::Next('-'));
        } else if prefix("startuptime") {
            *cursor += "startuptime".len();
            return Ok(Want::Next('-'));
        } else if prefix("clean") {
            self.clean = true;
            self.user_config = UserConfig::None;
            self.shada = ShadaConfig::None;
        } else if prefix("luamod-dev") {
            return Err(unsupported(argument, "the Lua module preload table"));
        } else if name.is_empty() {
            *had_minmin = true;
        } else {
            return Err(UsageError::about("Unknown option argument", argument));
        }
        Ok(Want::Argument)
    }

    /// Apply the separate argument that `letter` claimed.
    ///
    /// Returns whether that argument was consumed; `-S` before another option
    /// leaves it in place and falls back to the default session file.
    fn take_argument(
        &mut self,
        letter: char,
        option: &str,
        value: Option<&String>,
        args: &[String],
        next: usize,
    ) -> Result<bool, UsageError> {
        match letter {
            'c' => {
                let value = value.expect("a missing -c argument already failed");
                self.push_command(value.clone())?;
            }
            'S' => {
                // main.c: no argument, or an argument that is itself an
                // option, means the default session file.
                let session = value
                    .filter(|value| !value.starts_with('-'))
                    .map_or("Session.vim", String::as_str);
                self.push_command(format!("so {session}"))?;
                return Ok(value.is_some_and(|value| !value.starts_with('-')));
            }
            'i' => {
                let value = value.expect("a missing -i argument already failed");
                self.shada = if value == "NONE" {
                    ShadaConfig::None
                } else {
                    ShadaConfig::File(value.clone())
                };
            }
            'l' => {
                let value = value.expect("a missing -l argument already failed");
                // main.c: "-l" implies headless, silent, no swap file, and
                // skips user config unless one was already requested.
                self.headless = true;
                self.no_swap_file = true;
                if self.user_config == UserConfig::Default {
                    self.user_config = UserConfig::None;
                }
                if self.shada == ShadaConfig::Default {
                    self.shada = ShadaConfig::None;
                }
                self.lua_script =
                    Some(LuaScript { path: value.clone(), args: args[next + 1..].to_vec() });
            }
            's' => {
                let value = value.expect("a missing -s argument already failed");
                if self.scriptin.is_some() {
                    return Err(UsageError::script_error(format!(
                        "Attempt to open script file again: \"{option} {value}\""
                    )));
                }
                self.scriptin = Some(value.clone());
            }
            'u' => {
                let value = value.expect("a missing -u argument already failed");
                self.user_config = match value.as_str() {
                    "NONE" => UserConfig::None,
                    "NORC" => UserConfig::NoRc,
                    _ => UserConfig::File(value.clone()),
                };
            }
            // "-U {gvimrc}" is accepted and ignored, like upstream.
            'U' => {}
            'w' | 'W' => {
                return Err(unsupported(option, "script recording of typed keys"));
            }
            '-' => {
                let value = value.expect("a missing long-option argument already failed");
                if option.eq_ignore_ascii_case("--cmd") {
                    if self.pre_commands.len() >= MAX_ARG_CMDS {
                        return Err(too_many_commands());
                    }
                    self.pre_commands.push(value.clone());
                } else if option.eq_ignore_ascii_case("--listen") {
                    self.listen = Some(value.clone());
                } else {
                    self.startuptime = Some(value.clone());
                }
            }
            other => unreachable!("option -{other} does not take an argument"),
        }
        Ok(true)
    }

    /// Append a `+cmd`, `-c` or `-S` command, enforcing upstream's ceiling.
    fn push_command(&mut self, command: String) -> Result<(), UsageError> {
        if self.commands.len() >= MAX_ARG_CMDS {
            return Err(too_many_commands());
        }
        self.commands.push(command);
        Ok(())
    }

    /// The number of windows or tab pages to create for `files`.
    ///
    /// `main.c` `create_windows`: an explicit count wins, otherwise one per
    /// file, and never fewer than the single startup window.
    #[must_use]
    pub fn startup_window_count(&self) -> usize {
        if self.window_count > 0 {
            return self.window_count;
        }
        self.files.len().max(1)
    }
}

/// A recognized upstream option whose behavior needs a subsystem oxvim does
/// not have. Rejecting it keeps the flag detectable; silently accepting it
/// would make a script believe the effect happened.
fn unsupported(option: &str, requirement: &str) -> UsageError {
    UsageError::main_error(format!("Option not supported: \"{option}\": requires {requirement}"))
}

fn too_many_commands() -> UsageError {
    UsageError::main_error("Too many \"+command\", \"-c command\" or \"--cmd command\" arguments")
}

/// `main.c` `get_number_arg`: read the digits at `cursor`, or return
/// `default_value` when none are there. `cursor` advances past the digits.
fn number_argument(argument: &str, cursor: &mut usize, default_value: usize) -> usize {
    let digits = argument[*cursor..].bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return default_value;
    }
    let text = &argument[*cursor..*cursor + digits];
    *cursor += digits;
    // Upstream accumulates into an int and wraps; a value that cannot fit is
    // clamped instead of silently truncated to a small count.
    text.parse::<usize>().unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_supported_form() {
        struct Case {
            args: &'static [&'static str],
            check: fn(&Cli) -> bool,
        }
        let cases = [
            Case { args: &["-u", "NONE"], check: |c| c.user_config == UserConfig::None },
            Case { args: &["-u", "NORC"], check: |c| c.user_config == UserConfig::NoRc },
            Case { args: &["-u", "init.vim"], check: |c| c.user_config == UserConfig::File("init.vim".into()) },
            Case { args: &["-u", ""], check: |c| c.user_config == UserConfig::File(String::new()) },
            Case { args: &["-i", "NONE"], check: |c| c.shada == ShadaConfig::None },
            Case { args: &["-i", "state.shada"], check: |c| c.shada == ShadaConfig::File("state.shada".into()) },
            Case { args: &["--clean"], check: |c| c.clean && c.user_config == UserConfig::None },
            Case { args: &["--cleanfoo"], check: |c| c.clean },
            Case { args: &["--embed"], check: |c| c.embed },
            Case { args: &["--headless"], check: |c| c.headless },
            Case { args: &["--HEADLESS"], check: |c| c.headless },
            Case { args: &["--listen", "127.0.0.1:7777"], check: |c| c.listen.as_deref() == Some("127.0.0.1:7777") },
            Case { args: &["--literal", "file"], check: |c| c.files == ["file"] },
            Case { args: &["-e"], check: |c| c.batch == Some(BatchMode { silent: false, input_is_text: false }) },
            Case { args: &["-es"], check: |c| c.batch == Some(BatchMode { silent: true, input_is_text: false }) },
            Case { args: &["-E"], check: |c| c.batch == Some(BatchMode { silent: false, input_is_text: true }) },
            Case { args: &["-Es"], check: |c| c.batch == Some(BatchMode { silent: true, input_is_text: true }) },
            Case { args: &["-e", "-"], check: |c| c.batch == Some(BatchMode { silent: true, input_is_text: false }) && c.no_swap_file && !c.stdin_file },
            Case { args: &["-"], check: |c| c.stdin_file && c.files.is_empty() },
            Case { args: &["-s", "script"], check: |c| c.scriptin.as_deref() == Some("script") },
            Case { args: &["-s", "-"], check: |c| c.scriptin.as_deref() == Some("-") },
            Case { args: &["+set number"], check: |c| c.commands == ["set number"] },
            Case { args: &["+"], check: |c| c.commands == ["$"] },
            Case { args: &["-c", "echo 1"], check: |c| c.commands == ["echo 1"] },
            Case { args: &["-cecho 1"], check: |c| c.commands == ["echo 1"] },
            Case { args: &["--cmd", "set loadplugins"], check: |c| c.pre_commands == ["set loadplugins"] },
            Case { args: &["-V"], check: |c| c.verbose == Some(VerboseConfig { level: 10, file: None }) },
            Case { args: &["-V3"], check: |c| c.verbose == Some(VerboseConfig { level: 3, file: None }) },
            Case { args: &["-Vlog.txt"], check: |c| c.verbose == Some(VerboseConfig { level: 10, file: Some("log.txt".into()) }) },
            Case { args: &["-V3log.txt"], check: |c| c.verbose == Some(VerboseConfig { level: 3, file: Some("log.txt".into()) }) },
            Case { args: &["--api-info"], check: |c| c.api_info },
            Case { args: &["--help"], check: |c| c.help },
            Case { args: &["-h"], check: |c| c.help },
            Case { args: &["-?"], check: |c| c.help },
            Case { args: &["--version"], check: |c| c.version },
            Case { args: &["-v"], check: |c| c.version },
            Case { args: &["-R"], check: |c| c.readonly },
            Case { args: &["-m"], check: |c| c.no_write && !c.no_modifiable },
            Case { args: &["-M"], check: |c| c.no_write && c.no_modifiable },
            Case { args: &["-n"], check: |c| c.no_swap_file },
            Case { args: &["-b"], check: |c| c.binary },
            Case { args: &["-N", "-X", "-f", "-U", "gvimrc"], check: |c| c.files.is_empty() },
            Case { args: &["-o"], check: |c| c.window_layout == WindowLayout::Horizontal && c.window_count == 0 },
            Case { args: &["-O2"], check: |c| c.window_layout == WindowLayout::Vertical && c.window_count == 2 },
            Case { args: &["-p3"], check: |c| c.window_layout == WindowLayout::Tabs && c.window_count == 3 },
            Case { args: &["-w80"], check: |c| c.window_height == Some(80) },
            Case { args: &["--startuptime", "log"], check: |c| c.startuptime.as_deref() == Some("log") },
            Case { args: &["one", "two"], check: |c| c.files == ["one", "two"] },
            Case { args: &["--", "-mystery"], check: |c| c.files == ["-mystery"] },
            Case { args: &["--", "+cmd"], check: |c| c.files == ["+cmd"] && c.commands.is_empty() },
        ];
        for case in cases {
            let parsed = Cli::parse(case.args.iter().copied()).unwrap_or_else(|error| panic!("{:?}: {error}", case.args));
            assert!((case.check)(&parsed), "failed form: {:?}", case.args);
        }
    }

    #[test]
    fn clustered_short_options_apply_in_order() {
        let parsed = Cli::parse(["-Rnb"]).unwrap();
        assert!(parsed.readonly && parsed.no_swap_file && parsed.binary);

        let parsed = Cli::parse(["-Mn"]).unwrap();
        assert!(parsed.no_write && parsed.no_modifiable && parsed.no_swap_file);

        // A cluster may end in an option that takes the next argument.
        let parsed = Cli::parse(["-nc", "echo 1"]).unwrap();
        assert!(parsed.no_swap_file);
        assert_eq!(parsed.commands, ["echo 1"]);

        // A cluster may end in an option with an inline value.
        let parsed = Cli::parse(["-nV3"]).unwrap();
        assert!(parsed.no_swap_file);
        assert_eq!(parsed.verbose, Some(VerboseConfig { level: 3, file: None }));
    }

    #[test]
    fn post_commands_keep_argv_order_across_c_plus_and_session() {
        let parsed =
            Cli::parse(["-c", "one", "--cmd", "pre", "+two", "-cthree", "--cmd", "pre2", "-S", "s.vim"])
                .unwrap();
        assert_eq!(parsed.commands, ["one", "two", "three", "so s.vim"]);
        assert_eq!(parsed.pre_commands, ["pre", "pre2"]);
    }

    #[test]
    fn noplugin_and_session_flags_parse_with_upstream_semantics() {
        let parsed = Cli::parse(["--noplugin"]).unwrap();
        assert!(!parsed.loadplugins);

        let parsed = Cli::parse(["--noplugins"]).unwrap();
        assert!(!parsed.loadplugins);

        let parsed = Cli::parse(["-u", "NONE"]).unwrap();
        assert!(!parsed.loadplugins);

        let parsed = Cli::parse(["-u", "NONE", "--clean"]).unwrap();
        assert!(parsed.loadplugins);

        let parsed = Cli::parse(["-u", "NORC", "--noplugin"]).unwrap();
        assert!(!parsed.loadplugins);

        let parsed = Cli::parse(["-S", "session.vim"]).unwrap();
        assert_eq!(parsed.commands, ["so session.vim"]);

        let parsed = Cli::parse(["-S"]).unwrap();
        assert_eq!(parsed.commands, ["so Session.vim"]);

        let parsed = Cli::parse(["-S", "-u", "NONE"]).unwrap();
        assert_eq!(parsed.commands, ["so Session.vim"]);
        assert_eq!(parsed.user_config, UserConfig::None);

        let parsed = Cli::parse(["+echo 1", "-S", "session.vim", "+echo 2"]).unwrap();
        assert_eq!(parsed.commands, ["echo 1", "so session.vim", "echo 2"]);

        let parsed = Cli::parse(["-e", "-s", "-u", "NONE"]).unwrap();
        assert_eq!(parsed.batch, Some(BatchMode { silent: true, input_is_text: false }));
        assert_eq!(parsed.user_config, UserConfig::None);
        assert!(parsed.scriptin.is_none());

        let parsed = Cli::parse(["-e", "-s", "file"]).unwrap();
        assert_eq!(parsed.batch, Some(BatchMode { silent: true, input_is_text: false }));
        assert_eq!(parsed.files, ["file"]);
        assert!(parsed.scriptin.is_none());
    }

    #[test]
    fn lua_script_consumes_remaining_arguments_verbatim() {
        let parsed = Cli::parse(["-l", "script.lua", "--clean", "file"]).unwrap();
        assert_eq!(parsed.lua_script, Some(LuaScript { path: "script.lua".into(), args: vec!["--clean".into(), "file".into()] }));
        assert!(!parsed.clean);
        // main.c: "-l" implies headless, no swap file and no user config.
        assert!(parsed.headless && parsed.no_swap_file);
        assert_eq!(parsed.user_config, UserConfig::None);
    }

    #[test]
    fn startup_window_count_follows_files_or_explicit_count() {
        assert_eq!(Cli::parse(["-o", "a", "b", "c"]).unwrap().startup_window_count(), 3);
        assert_eq!(Cli::parse(["-o2", "a", "b", "c"]).unwrap().startup_window_count(), 2);
        assert_eq!(Cli::parse(["-o5", "a", "b", "c"]).unwrap().startup_window_count(), 5);
        assert_eq!(Cli::parse(["-o"]).unwrap().startup_window_count(), 1);
        assert_eq!(Cli::parse(["-p3"]).unwrap().startup_window_count(), 3);
    }

    #[test]
    fn reports_upstream_error_text_and_status() {
        struct Case {
            args: &'static [&'static str],
            message: &'static str,
            code: u8,
        }
        let cases = [
            Case { args: &["--unknown"], message: "Unknown option argument: \"--unknown\"", code: 1 },
            Case { args: &["-Q"], message: "Unknown option argument: \"-Q\"", code: 1 },
            Case { args: &["-u"], message: "Argument missing after: \"-u\"", code: 1 },
            Case { args: &["-c"], message: "Argument missing after: \"-c\"", code: 1 },
            Case { args: &["-i"], message: "Argument missing after: \"-i\"", code: 1 },
            Case { args: &["--listen"], message: "Argument missing after: \"--listen\"", code: 1 },
            Case { args: &["--cmd"], message: "Argument missing after: \"--cmd\"", code: 1 },
            Case { args: &["--startuptime"], message: "Argument missing after: \"--startuptime\"", code: 1 },
            Case { args: &["-l"], message: "Argument missing after: \"-l\"", code: 1 },
            Case { args: &["-uxx", "NONE"], message: "Garbage after option argument: \"-uxx\"", code: 1 },
            Case { args: &["--cmdfoo", "x"], message: "Garbage after option argument: \"--cmdfoo\"", code: 1 },
            Case { args: &["-s", "a", "-s", "b"], message: "Attempt to open script file again: \"-s b\"", code: 2 },
            Case { args: &["--embed", "-es"], message: "--embed conflicts with -es/-Es/-l", code: 1 },
            Case { args: &["-d"], message: "Option not supported: \"-d\": requires a diff engine", code: 1 },
        ];
        for case in cases {
            let error = Cli::parse(case.args.iter().copied())
                .expect_err(&format!("expected error for {:?}", case.args));
            assert!(
                format!("{error}").contains(case.message),
                "{:?}: {error}",
                case.args
            );
            assert_eq!(error.exit_code(), case.code, "{:?}", case.args);
        }
    }

    #[test]
    fn command_count_ceiling_matches_max_arg_cmds() {
        let ten = ["+echo"; MAX_ARG_CMDS];
        assert_eq!(Cli::parse(ten).unwrap().commands.len(), MAX_ARG_CMDS);

        let eleven = ["+echo"; MAX_ARG_CMDS + 1];
        let error = Cli::parse(eleven).expect_err("eleven commands must fail");
        assert!(format!("{error}").contains("Too many \"+command\""), "{error}");

        let mut pre = Vec::new();
        for _ in 0..=MAX_ARG_CMDS {
            pre.push("--cmd");
            pre.push("echo");
        }
        assert!(Cli::parse(pre).is_err());
    }

    #[test]
    fn unsupported_options_are_rejected_with_their_missing_subsystem() {
        for (args, requirement) in [
            (vec!["-d"], "a diff engine"),
            (vec!["-A"], "the 'arabic' option side effects and keymap files"),
            (vec!["-H"], "keymap file loading"),
            (vec!["-D"], "the Ex debugger"),
            (vec!["-q", "errors"], "the quickfix list and 'errorformat'"),
            (vec!["-t", "tag"], "the tags subsystem"),
            (vec!["-r"], "swap-file recovery"),
            (vec!["-L"], "swap-file recovery"),
            (vec!["-w", "keys.log"], "script recording of typed keys"),
            (vec!["-W", "keys.log"], "script recording of typed keys"),
            (vec!["--remote", "file"], "RPC client channels and vim._cs_remote"),
            (vec!["--remote-expr", "1"], "RPC client channels and vim._cs_remote"),
            (vec!["--server", "addr"], "RPC client channels and vim._cs_remote"),
            (vec!["--luamod-dev"], "the Lua module preload table"),
        ] {
            let error = Cli::parse(args.clone()).expect_err(&format!("expected error for {args:?}"));
            let text = format!("{error}");
            assert!(text.contains("Option not supported"), "{args:?}: {text}");
            assert!(text.contains(requirement), "{args:?}: {text}");
            assert_eq!(error.exit_code(), 1, "{args:?}");
        }
    }
}
