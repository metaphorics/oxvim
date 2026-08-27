//! Filesystem builtins: the path-and-file family delegates to
//! [`crate::fs_builtins`]; `swapfilelist` additionally needs the `directory`
//! option its `recover_names` scan walks.

use ox_types::Typval;

use crate::options::OptionValue;
use crate::script::FileIO;

use crate::excmd_exec::EvalHost;

/// Routes one filesystem builtin.
pub(crate) fn call<F: FileIO>(
    host: &mut EvalHost<'_, F>,
    name: &str,
    args: Vec<Typval>,
) -> ox_eval::Result<Typval> {
    if name == "swapfilelist" {
        // f_swapfilelist iterates the 'directory' option (recover_names); the
        // option has no static default upstream (it is computed at startup),
        // so an unset store reads as "." — the first entry of every platform
        // default.
        let directory = match host.editor.options().get_global("directory") {
            Ok(OptionValue::String(value)) => value.clone(),
            _ => ".".to_owned(),
        };
        return crate::fs_builtins::swapfilelist(host.runtime.scripts.io(), args.len(), &directory);
    }
    if name == "writefile" {
        // The `D` flag is `add_defer("delete", ...)` against the enclosing
        // function frame, so this one needs the runtime and not just the
        // `FileIO` seam.
        crate::fs_builtins::check_writefile_arity(args.len())?;
        let mut deferred = None;
        let in_function = host.runtime.can_add_defer();
        let result = crate::fs_builtins::writefile(
            host.runtime.scripts.io(),
            &args,
            in_function,
            &mut deferred,
        );
        if let Some(path) = deferred {
            host.runtime.push_deferred_delete(path, crate::fs_builtins::DeleteMode::File);
        }
        return result;
    }
    if name == "mkdir" {
        // The `D`/`R` flags are `add_defer("delete", dir, 'd'/'rf')` against
        // the enclosing function frame, so `mkdir` needs the runtime like
        // `writefile` does.
        crate::fs_builtins::check_arity("mkdir", args.len())?;
        let mut deferred = None;
        let result = crate::fs_builtins::mkdir(
            host.runtime.scripts.io(),
            &args,
            host.runtime.can_add_defer(),
            &mut deferred,
        );
        if let Some((path, mode)) = deferred {
            host.runtime.push_deferred_delete(path, mode);
        }
        return result;
    }
    crate::fs_builtins::call(host.runtime.scripts.io(), name, args)
}
