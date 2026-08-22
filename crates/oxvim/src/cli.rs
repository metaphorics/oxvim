//! Command-line parsing for the `oxvim` binary.

use std::fmt;

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

/// Non-interactive Ex input mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BatchMode {
    /// Ex mode (`-e`).
    Ex,
    /// Silent Ex mode (`-es`).
    SilentEx,
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
            loadplugins: true,
            files: Vec::new(),
        }
    }
}

/// A command-line usage failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageError {
    message: String,
}

impl UsageError {
    fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

impl fmt::Display for UsageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}\nRun 'oxvim --help' for usage information.", self.message)
    }
}

impl std::error::Error for UsageError {}

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
        let mut options = true;

        while index < args.len() {
            let argument = &args[index];
            if options && argument == "--" {
                options = false;
                index += 1;
                continue;
            }
            if options && argument == "-u" {
                let value = required_value(&args, &mut index, "-u")?;
                cli.user_config = match value.as_str() {
                    "NONE" => UserConfig::None,
                    "NORC" => UserConfig::NoRc,
                    _ => UserConfig::File(value),
                };
            } else if options && argument == "-i" {
                let value = required_value(&args, &mut index, "-i")?;
                cli.shada = if value == "NONE" { ShadaConfig::None } else { ShadaConfig::File(value) };
            } else if options && argument == "--clean" {
                cli.clean = true;
            } else if options && (argument == "--noplugin" || argument == "--noplugins") {
                cli.loadplugins = false;
            } else if options && argument == "--embed" {
                cli.embed = true;
            } else if options && argument == "--headless" {
                cli.headless = true;
            } else if options && argument == "--listen" {
                cli.listen = Some(required_value(&args, &mut index, "--listen")?);
            } else if options && argument == "-l" {
                let path = required_value(&args, &mut index, "-l")?;
                cli.lua_script = Some(LuaScript { path, args: args[index + 1..].to_vec() });
                index = args.len();
                continue;
            } else if options && argument == "-es" {
                set_batch_mode(&mut cli, BatchMode::SilentEx)?;
            } else if options && argument == "-e" {
                set_batch_mode(&mut cli, BatchMode::Ex)?;
            } else if options && argument == "-s" {
                if cli.batch.is_some() {
                    // `-s` after `-e` or `-es` is the silent batch modifier.
                    cli.batch = Some(BatchMode::SilentEx);
                } else {
                    if cli.scriptin.is_some() {
                        return Err(UsageError::new("Only one -s script may be given"));
                    }
                    cli.scriptin = Some(required_value(&args, &mut index, "-s")?);
                }
            } else if options && argument == "-S" {
                let file = if let Some(next) = args.get(index + 1) {
                    if next.starts_with('-') {
                        "Session.vim".to_owned()
                    } else {
                        index += 1;
                        next.clone()
                    }
                } else {
                    "Session.vim".to_owned()
                };
                cli.commands.push(format!("so {file}"));
            } else if options && argument == "--cmd" {
                cli.pre_commands.push(required_value(&args, &mut index, "--cmd")?);
            } else if options && argument == "--api-info" {
                cli.api_info = true;
            } else if options && argument.starts_with("-V") {
                cli.verbose = Some(parse_verbose(&argument[2..])?);
            } else if options && argument.starts_with('+') {
                cli.commands.push(if argument.len() == 1 { "$".to_owned() } else { argument[1..].to_owned() });
            } else if options && argument.starts_with('-') && argument != "-" {
                return Err(UsageError::new(format!("Unknown option: {argument}")));
            } else {
                cli.files.push(argument.clone());
            }
            index += 1;
        }

        // Upstream main.c: `-u NONE` resets 'loadplugins' to false, unless
        // `--clean` was also given (`p_lpl = vimrc_none ? params.clean : p_lpl`).
        if cli.user_config == UserConfig::None {
            cli.loadplugins = cli.clean;
        }

        if cli.embed && (cli.batch.is_some() || cli.lua_script.is_some() || cli.scriptin.is_some()) {
            return Err(UsageError::new("--embed conflicts with -e/-es/-s/-l"));
        }
        Ok(cli)
    }
}

fn required_value(args: &[String], index: &mut usize, option: &str) -> Result<String, UsageError> {
    *index += 1;
    args.get(*index)
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| UsageError::new(format!("Argument missing after: {option}")))
}

fn set_batch_mode(cli: &mut Cli, mode: BatchMode) -> Result<(), UsageError> {
    if cli.batch.replace(mode).is_some() {
        return Err(UsageError::new("Only one of -e and -es may be used"));
    }
    Ok(())
}

fn parse_verbose(suffix: &str) -> Result<VerboseConfig, UsageError> {
    if suffix.is_empty() {
        return Ok(VerboseConfig { level: 10, file: None });
    }
    let digits = suffix.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return Ok(VerboseConfig { level: 10, file: Some(suffix.to_owned()) });
    }
    let level = suffix[..digits]
        .parse::<u32>()
        .map_err(|_| UsageError::new(format!("Invalid argument: -V{suffix}")))?;
    let file = &suffix[digits..];
    Ok(VerboseConfig { level, file: if file.is_empty() { None } else { Some(file.to_owned()) } })
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
            Case { args: &["-i", "NONE"], check: |c| c.shada == ShadaConfig::None },
            Case { args: &["-i", "state.shada"], check: |c| c.shada == ShadaConfig::File("state.shada".into()) },
            Case { args: &["--clean"], check: |c| c.clean },
            Case { args: &["--embed"], check: |c| c.embed },
            Case { args: &["--headless"], check: |c| c.headless },
            Case { args: &["--listen", "127.0.0.1:7777"], check: |c| c.listen.as_deref() == Some("127.0.0.1:7777") },
            Case { args: &["-e"], check: |c| c.batch == Some(BatchMode::Ex) },
            Case { args: &["-es"], check: |c| c.batch == Some(BatchMode::SilentEx) },
            Case { args: &["-s", "script"], check: |c| c.scriptin.as_deref() == Some("script") },
            Case { args: &["-s", "-"], check: |c| c.scriptin.as_deref() == Some("-") },
            Case { args: &["+set number"], check: |c| c.commands == ["set number"] },
            Case { args: &["+"], check: |c| c.commands == ["$"] },
            Case { args: &["--cmd", "set loadplugins"], check: |c| c.pre_commands == ["set loadplugins"] },
            Case { args: &["-V"], check: |c| c.verbose == Some(VerboseConfig { level: 10, file: None }) },
            Case { args: &["-V3"], check: |c| c.verbose == Some(VerboseConfig { level: 3, file: None }) },
            Case { args: &["-Vlog.txt"], check: |c| c.verbose == Some(VerboseConfig { level: 10, file: Some("log.txt".into()) }) },
            Case { args: &["-V3log.txt"], check: |c| c.verbose == Some(VerboseConfig { level: 3, file: Some("log.txt".into()) }) },
            Case { args: &["--api-info"], check: |c| c.api_info },
            Case { args: &["one", "two"], check: |c| c.files == ["one", "two"] },
            Case { args: &["--", "-mystery"], check: |c| c.files == ["-mystery"] },
        ];
        for case in cases {
            let parsed = Cli::parse(case.args.iter().copied()).unwrap_or_else(|error| panic!("{:?}: {error}", case.args));
            assert!((case.check)(&parsed), "failed form: {:?}", case.args);
        }
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
        assert_eq!(parsed.batch, Some(BatchMode::SilentEx));
        assert_eq!(parsed.user_config, UserConfig::None);
        assert!(parsed.scriptin.is_none());

        let parsed = Cli::parse(["-e", "-s", "file"]).unwrap();
        assert_eq!(parsed.batch, Some(BatchMode::SilentEx));
        assert_eq!(parsed.files, ["file"]);
        assert!(parsed.scriptin.is_none());
    }

    #[test]
    fn lua_script_consumes_remaining_arguments_verbatim() {
        let parsed = Cli::parse(["-l", "script.lua", "--clean", "file"]).unwrap();
        assert_eq!(parsed.lua_script, Some(LuaScript { path: "script.lua".into(), args: vec!["--clean".into(), "file".into()] }));
        assert!(!parsed.clean);
    }

    #[test]
    fn reports_unknown_missing_and_conflicting_arguments() {
        for args in [
            vec!["--unknown"],
            vec!["-u"],
            vec!["-i"],
            vec!["--listen"],
            vec!["-l"],
            vec!["-s"],
            vec!["-s", "a", "-s", "b"],
            vec!["-e", "-es"],
            vec!["--embed", "-es"],
            vec!["--embed", "-s", "script"],
        ] {
            assert!(Cli::parse(args.clone()).is_err(), "expected error for {args:?}");
        }
    }
}
