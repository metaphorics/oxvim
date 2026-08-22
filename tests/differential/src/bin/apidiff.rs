#![allow(missing_docs)]

use differential::{ORACLE, OXVIM, api_info, binary, normalize_api, readable_diff};

fn main() {
    if let Err(error) = run() {
        eprintln!("apidiff: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let oracle = normalize_api(api_info(&binary(ORACLE))?);
    let oxvim = normalize_api(api_info(&binary(OXVIM))?);
    if oracle == oxvim {
        println!("apidiff: schemas match (version.build ignored)");
        return Ok(());
    }
    Err(format!(
        "semantic API mismatch (only version.build is allowed)\n{}",
        readable_diff("neovim --api-info", &oracle, "oxvim --api-info", &oxvim)
    ))
}
