#![allow(missing_docs)]

use std::fs;
use std::path::{Path, PathBuf};

use differential::{
    ORACLE, OXVIM, binary, divergence_fingerprint, load_session, readable_diff, read_skips, run_session,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("replay: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut bless = false;
    let mut reason = None;
    let mut selected = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--bless" => bless = true,
            "--reason" => reason = args.next(),
            _ if argument.starts_with('-') => return Err(format!("unknown option: {argument}")),
            _ => selected.push(PathBuf::from(argument)),
        }
    }
    if bless {
        let value = reason.as_deref().ok_or_else(|| "--bless requires a non-empty --reason justification".to_owned())?;
        if value.trim().is_empty() || value.contains(['\r', '\n']) {
            return Err("--reason must be a non-empty single line".to_owned());
        }
        reason = Some(value.trim().to_owned());
    }
    if selected.is_empty() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("replay/sessions");
        selected = fs::read_dir(&directory)
            .map_err(|error| format!("could not read {}: {error}", directory.display()))?
            .map(|entry| entry.map(|entry| entry.path()).map_err(|error| error.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        selected.retain(|path| path.extension().and_then(|value| value.to_str()) == Some("yaml"));
        selected.sort();
    }

    let mut skips = read_skips().map_err(|error| format!("could not read SKIPS.md: {error}"))?;
    let mut failed = false;
    for path in selected {
        let path = if path.is_absolute() { path } else { Path::new(env!("CARGO_MANIFEST_DIR")).join(path) };
        let steps = load_session(&path)?;
        let oracle = rmpv::Value::Array(run_session(&binary(ORACLE), &steps)?);
        let oxvim = rmpv::Value::Array(run_session(&binary(OXVIM), &steps)?);
        let name = path.strip_prefix(env!("CARGO_MANIFEST_DIR")).unwrap_or(&path).display().to_string();
        if oracle == oxvim {
            let count = oracle.as_array().map_or(0, Vec::len);
            println!("PASS {name} ({count} stream events)");
            continue;
        }

        let fingerprint = divergence_fingerprint(&oracle, &oxvim);
        let session_prefix = format!("- {name} [sha256:");
        let legacy_prefix = format!("- {name} — ");
        let key = format!("{session_prefix}{fingerprint}] — ");
        let diff = readable_diff("neovim stream", &oracle, "oxvim stream", &oxvim);
        if bless {
            let line = format!("{key}{}", reason.as_deref().unwrap_or_default());
            skips.retain(|existing| !existing.starts_with(&session_prefix) && !existing.starts_with(&legacy_prefix));
            skips.push(line);
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("SKIPS.md");
            fs::write(&path, format!("{}\n", skips.join("\n")))
                .map_err(|error| format!("could not rewrite {}: {error}", path.display()))?;
            println!("BLESSED {name} [{fingerprint}]\n{diff}");
        } else if skips.iter().any(|line| line.starts_with(&key) && line.len() > key.len()) {
            println!("SANCTIONED {name} [{fingerprint}]\n{diff}");
        } else {
            eprintln!("FAIL {name}\n{diff}");
            failed = true;
        }
    }
    if failed { Err("unsanctioned session divergences found".to_owned()) } else { Ok(()) }
}
