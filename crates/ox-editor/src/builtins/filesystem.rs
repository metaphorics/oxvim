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
    crate::fs_builtins::call(host.runtime.scripts.io(), name, args)
}
