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
    /// Script input mode (`-s`).
    Script,
}

/// A Lua script and the arguments following it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LuaScript {
    /// Script filename.
    pub path: String,
    /// Arguments exposed as `arg[1]` onward.
    pub args: Vec<String>,
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
    /// Commands executed before configuration.
    pub pre_commands: Vec<String>,
    /// Commands executed after configuration.
    pub commands: Vec<String>,
    /// Verbosity level requested with `-V`.
    pub verbose: Option<u32>,
    /// Print API metadata and exit.
    pub api_info: bool,
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
            pre_commands: Vec::new(),
            commands: Vec::new(),
            verbose: None,
            api_info: false,
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
                set_batch_mode(&mut cli, BatchMode::Script)?;
            } else if options && argument == "--cmd" {
                cli.pre_commands.push(required_value(&args, &mut index, "--cmd")?);
            } else if options && argument == "--api-info" {
                cli.api_info = true;
            } else if options && argument.starts_with("-V") {
                let level = &argument[2..];
                cli.verbose = Some(if level.is_empty() {
                    10
                } else {
                    level.parse().map_err(|_| UsageError::new(format!("Invalid argument: {argument}")))?
                });
            } else if options && argument.starts_with('+') {
                cli.commands.push(if argument.len() == 1 { "$".to_owned() } else { argument[1..].to_owned() });
            } else if options && argument.starts_with('-') && argument != "-" {
                return Err(UsageError::new(format!("Unknown option: {argument}")));
            } else {
                cli.files.push(argument.clone());
            }
            index += 1;
        }

        if cli.embed && (cli.batch.is_some() || cli.lua_script.is_some()) {
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
        return Err(UsageError::new("Only one of -e, -es, and -s may be used"));
    }
    Ok(())
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
            Case { args: &["-s"], check: |c| c.batch == Some(BatchMode::Script) },
            Case { args: &["+set number"], check: |c| c.commands == ["set number"] },
            Case { args: &["+"], check: |c| c.commands == ["$"] },
            Case { args: &["--cmd", "set loadplugins"], check: |c| c.pre_commands == ["set loadplugins"] },
            Case { args: &["-V"], check: |c| c.verbose == Some(10) },
            Case { args: &["-V3"], check: |c| c.verbose == Some(3) },
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
            vec!["-Vx"],
            vec!["-e", "-s"],
            vec!["--embed", "-es"],
        ] {
            assert!(Cli::parse(args).is_err());
        }
    }
}
