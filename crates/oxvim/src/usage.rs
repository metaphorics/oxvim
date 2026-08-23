//! `--help` and `--version` output.
//!
//! `main.c` prints both from inside `command_line_scan` and calls
//! `os_exit(0)`, so each is a successful terminal action rather than a startup
//! mode. Only the flags oxvim actually acts on are listed: a help screen that
//! advertised a rejected flag would be a lie a script cannot detect.

use crate::api_info;
use crate::AppError;
use ox_types::Object;

/// The one-line usage summary plus every supported option.
pub const HELP: &str = "\
Usage:
  oxvim [options] [file ...]

Options:
  --cmd <cmd>           Execute <cmd> before any config
  +<cmd>, -c <cmd>      Execute <cmd> after config and first file
  -l <script> [args...] Execute Lua <script> (with optional args)
  -S <session>          Source <session> after loading the first file
  -s <scriptin>         Read Normal mode commands from <scriptin>
  -u <config>           Use this config file

  -es, -Es              Silent (batch) mode
  -h, --help            Print this help message
  -i <shada>            Use this shada file
  -v, --version         Print version information
  -V[N][file]           Verbose [level][file]

  --                    Only file names after this
  --api-info            Write msgpack-encoded API metadata to stdout
  --clean               \"Factory defaults\" (skip user config and plugins, shada)
  --embed               Use stdin/stdout as a msgpack-rpc channel
  --headless            Don't start a user interface
  --listen <address>    Serve RPC API from this address
  --noplugin            Skip loading plugins
";

/// Version and API level, read from the same canonical metadata that
/// `--api-info` encodes so the two can never disagree.
pub fn version() -> Result<String, AppError> {
    let metadata = api_info::metadata().map_err(|error| AppError::Api(error.to_string()))?;
    let version = field(&metadata, "version")
        .ok_or_else(|| AppError::Api("API metadata has no version".into()))?;
    let number = |name: &str| match field(version, name) {
        Some(Object::Integer(value)) => Ok(*value),
        _ => Err(AppError::Api(format!("API metadata has no integer version.{name}"))),
    };
    Ok(format!(
        "OXVIM v{}.{}.{}\nAPI level {} (compatible: {})\nBuild type: {}\n",
        number("major")?,
        number("minor")?,
        number("patch")?,
        number("api_level")?,
        number("api_compatible")?,
        if cfg!(debug_assertions) { "Debug" } else { "Release" },
    ))
}

fn field<'a>(value: &'a Object, name: &str) -> Option<&'a Object> {
    let Object::Dict(dict) = value else { return None };
    dict.0
        .iter()
        .find(|(key, _)| key.as_bytes() == name.as_bytes())
        .map(|(_, value)| value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_reports_the_api_level_from_canonical_metadata() {
        let text = version().unwrap();
        assert!(text.starts_with("OXVIM v0.13.0\n"), "{text}");
        assert!(text.contains("API level 15 (compatible: 0)"), "{text}");
    }

    #[test]
    fn help_lists_only_options_the_parser_accepts() {
        // Every flag advertised here must parse; a rejected flag in the help
        // screen would promise an effect that never happens.
        for (advertised, invocation) in [
            ("-es, -Es", "-es"),
            ("-h, --help", "-h"),
            ("-v, --version", "-v"),
            ("--clean", "--clean"),
            ("--headless", "--headless"),
            ("--noplugin", "--noplugin"),
        ] {
            assert!(HELP.contains(advertised), "help omits {advertised}");
            let parsed = crate::cli::Cli::parse([invocation, "log"]);
            assert!(parsed.is_ok(), "help advertises rejected {invocation}");
        }
        for rejected in ["-d ", "--remote", "--server", "-q ", "-t ", "-W "] {
            assert!(!HELP.contains(rejected), "help advertises rejected {rejected}");
        }
    }
}
