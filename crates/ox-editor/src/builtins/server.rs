//! `serverstart()`, `serverstop()` and `serverlist()` — the listening-RPC
//! server builtins (upstream `f_serverstart`/`f_serverstop`/`f_serverlist`,
//! `eval/funcs.c:6194-6279`, over `msgpack_rpc/server.c`).
//!
//! Every address question is answered by [`crate::server::ServerHost`], the
//! process-level listen machinery the embedder installs on the runtime; the
//! builtins themselves own argument validation, the returned values, and the
//! `v:servername` bookkeeping upstream does inside `server_start`/`server_stop`
//! (`server.c:206-209,250-253`).

use crate::excmd_exec::{EvalHost, ExEditorAccess};
use crate::script::FileIO;
use crate::server::{prepare_server_address, server_address_new};
use ox_eval::{EvalError, Scope, ScopeKind};
use ox_types::{OxStr, Typval};

/// Routes one server builtin.
///
/// Every name [`super::route`] sends to [`super::Family::Server`] is served
/// here.
pub(crate) fn call<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    name: &str,
    args: &[Typval],
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    match name {
        "serverstart" => server_start(host, scope, args),
        "serverstop" => server_stop(host, scope, args),
        "serverlist" => server_list(host),
        _ => unreachable!("server builtin route and dispatcher disagree"),
    }
}

/// `serverstart([{address}])` (`eval/funcs.c:6218-6259`): starts listening and
/// returns the final bound address — a generated per-process address when the
/// argument is missing or a bare name (`server.c:173-174`, #8519), the
/// resolved `host:port` when a random TCP port was assigned.
fn server_start<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    scope: &mut Scope,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    let address = match args.first() {
        None => server_address_new(None),
        // An empty address is WLOG-failed upstream (server.c:168-171) and
        // surfaces as the generic start error (eval/funcs.c:6240-6246),
        // never as a hidden generated name.
        Some(Typval::String(value)) if value.as_bytes().is_empty() => {
            return Err(start_failed("Unknown system error"));
        }
        // `f_serverstart` rejects every non-String argument with `e_invarg`
        // (`eval/funcs.c:6230-6233`); Numbers are not coerced here.
        Some(Typval::String(value)) => prepare_server_address(&value.to_string_lossy()),
        Some(_) => return Err(EvalError::new("E474", 0, "Invalid argument")),
    };
    let Some(server) = host.runtime.servers.as_mut() else {
        return Err(start_failed("Unknown system error"));
    };
    let bound = server.start(&address).map_err(start_failed)?;
    // v:servername only changes when unset (`server.c:206-209`). The write
    // goes through the scope: the statement-end sync pushes scope-owned `v:`
    // pairs back into the editor map, so a direct `vvars_mut` write would be
    // clobbered by a dirty `v:` push.
    let unset = match scope.get_scoped(ScopeKind::Vim, b"servername", 0) {
        Ok(Typval::String(value)) => value.as_bytes().is_empty(),
        _ => true,
    };
    if unset {
        scope.replace_pair(
            ScopeKind::Vim,
            "servername",
            Typval::String(OxStr::from(bound.as_str())),
        );
    }
    Ok(Typval::String(OxStr::from(bound.as_str())))
}

/// `serverstop({address})` (`eval/funcs.c:6262-6279`): 1 when a listener was
/// stopped, 0 for the empty string or an address nothing listens on; a
/// non-String argument is `e_invarg` (`eval/funcs.c:6268-6271`).
fn server_stop<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    scope: &mut Scope,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    let Some(Typval::String(value)) = args.first() else {
        return Err(EvalError::new("E474", 0, "Invalid argument"));
    };
    let address = value.to_string_lossy();
    if address.is_empty() {
        return Ok(Typval::Number(0));
    }
    let Some(server) = host.runtime.servers.as_mut() else {
        return Ok(Typval::Number(0));
    };
    if !server.stop(&address) {
        return Ok(Typval::Number(0));
    }
    // Stopping the `v:servername` listener bumps it to the next available
    // server, or clears it when none remain (`server.c:250-253`). Same
    // scope-routed write as [`server_start`].
    let remaining = server.list();
    let is_current = matches!(
        scope.get_scoped(ScopeKind::Vim, b"servername", 0),
        Ok(Typval::String(current)) if current.as_bytes() == address.as_ref().as_bytes()
    );
    if is_current {
        let next = remaining.first().map(String::as_str).unwrap_or_default();
        scope.replace_pair(
            ScopeKind::Vim,
            "servername",
            Typval::String(OxStr::from(next)),
        );
    }
    Ok(Typval::Number(1))
}

/// `serverlist()` (`eval/funcs.c:6195-6215`): this process's live listener
/// addresses. Upstream also routes an options dictionary through Lua for peer
/// discovery (`vim._core.server`); discovery is not modeled here, so the
/// argument is accepted and ignored and the own-address list is returned.
#[expect(
    clippy::unnecessary_wraps,
    reason = "builtin dispatch table requires Result"
)]
fn server_list<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
) -> ox_eval::Result<Typval> {
    let addresses = host
        .runtime
        .servers
        .as_ref()
        .map_or(Vec::new(), |server| server.list());
    Ok(Typval::list(
        addresses
            .into_iter()
            .map(|address| Typval::String(OxStr::from(address.as_str())))
            .collect(),
    ))
}

/// Upstream reports a failed start code-less through `semsg`
/// (`eval/funcs.c:6242-6245`). Every `EvalError` carries a code, so the
/// message rides on E900, this port's RPC-domain channel (the same code the
/// neighboring `job`/`channel` builtins use).
fn start_failed(suffix: impl std::fmt::Display) -> EvalError {
    EvalError::new("E900", 0, format!("Failed to start server: {suffix}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::ServerHost;
    use crate::{Editor, ExExecutor, ExecError, TestEditorAccess, VimExceptionKind};
    use ox_eval::Scope;
    use ox_types::Object;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A `ServerHost` that records calls and answers from an in-memory
    /// address book — no sockets, no loop.
    struct FakeServerHost {
        state: Rc<RefCell<FakeState>>,
    }

    struct FakeState {
        calls: Vec<String>,
        addresses: Vec<String>,
    }

    impl ServerHost for FakeServerHost {
        fn start(&mut self, address: &str) -> Result<String, String> {
            let mut state = self.state.borrow_mut();
            state.calls.push(format!("start:{address}"));
            if state.addresses.iter().any(|a| a == address) {
                return Err("Unknown system error".into());
            }
            state.addresses.push(address.to_owned());
            Ok(address.to_owned())
        }

        fn stop(&mut self, address: &str) -> bool {
            let mut state = self.state.borrow_mut();
            state.calls.push(format!("stop:{address}"));
            state
                .addresses
                .iter()
                .position(|a| a == address)
                .is_some_and(|index| {
                    state.addresses.remove(index);
                    true
                })
        }

        fn list(&self) -> Vec<String> {
            self.state.borrow().addresses.clone()
        }
    }

    fn harness() -> (TestEditorAccess, ExExecutor, Rc<RefCell<FakeState>>) {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        let state = Rc::new(RefCell::new(FakeState {
            calls: Vec::new(),
            addresses: Vec::new(),
        }));
        exec.set_server_host(Box::new(FakeServerHost {
            state: Rc::clone(&state),
        }));
        (editor, exec, state)
    }

    fn servername(editor: &TestEditorAccess) -> Option<String> {
        editor
            .editor()
            .vvars()
            .get(&OxStr::from("servername"))
            .and_then(|value| match value {
                Object::String(text) => Some(text.to_string_lossy().into_owned()),
                _ => None,
            })
    }

    fn global(scope: &Scope, name: &str) -> Option<Typval> {
        scope
            .get_scoped(ox_eval::ScopeKind::Global, name.as_bytes(), 0)
            .ok()
            .cloned()
    }

    fn global_number(scope: &Scope, name: &str) -> Option<i64> {
        match global(scope, name)? {
            Typval::Number(value) => Some(value),
            Typval::Bool(value) => Some(i64::from(value)),
            _ => None,
        }
    }

    fn error_code(err: &ExecError) -> String {
        match err {
            ExecError::Vim(exception) => match &exception.kind {
                VimExceptionKind::Error(code) => code.clone(),
                VimExceptionKind::Throw => "Throw".to_owned(),
            },
            other => panic!("expected Vim error, got {other:?}"),
        }
    }

    #[test]
    fn serverstart_returns_the_bound_address_and_sets_v_servername() {
        let (editor, mut exec, _state) = harness();
        exec.execute_script(
            &editor,
            "<test>",
            "let g:addr = serverstart('/tmp/Xtest-sock')",
        )
        .unwrap();
        assert_eq!(servername(&editor).as_deref(), Some("/tmp/Xtest-sock"));
        match global(exec.scope(), "addr") {
            Some(Typval::String(text)) => {
                assert_eq!(text.to_string_lossy(), "/tmp/Xtest-sock");
            }
            other => panic!("expected string address, got {other:?}"),
        }
    }

    #[test]
    fn serverstart_without_address_generates_one() {
        let (editor, mut exec, _state) = harness();
        exec.execute_script(&editor, "<test>", "let g:addr = serverstart()")
            .unwrap();
        match global(exec.scope(), "addr") {
            Some(Typval::String(text)) => {
                let addr = text.to_string_lossy().into_owned();
                assert!(addr.contains("/nvim."), "{addr}");
                assert!(
                    addr.contains(&format!(".{}.", std::process::id())),
                    "{addr}"
                );
                assert_eq!(servername(&editor).as_deref(), Some(addr.as_str()));
            }
            other => panic!("expected string address, got {other:?}"),
        }
    }
    #[test]
    fn serverstart_bare_name_expands_to_generated_path() {
        let (editor, mut exec, state) = harness();
        exec.execute_script(
            &editor,
            "<test>",
            "let g:addr = serverstart('xtest1.2.3.4')",
        )
        .unwrap();
        let addr = state.borrow().addresses[0].clone();
        assert!(addr.contains("/xtest1.2.3.4."), "{addr}");
    }

    #[test]
    fn serverstart_rejects_non_string_argument() {
        let (editor, mut exec, _state) = harness();
        let err = exec
            .execute_script(&editor, "<test>", "call serverstart(1)")
            .unwrap_err();
        assert_eq!(error_code(&err), "E474");
    }

    #[test]
    fn serverstart_duplicate_returns_error() {
        let (editor, mut exec, _state) = harness();
        exec.execute_script(&editor, "<test>", "call serverstart('/tmp/Xtest-dup')")
            .unwrap();
        let err = exec
            .execute_script(&editor, "<test>", "call serverstart('/tmp/Xtest-dup')")
            .unwrap_err();
        assert_eq!(error_code(&err), "E900");
    }

    #[test]
    fn serverstart_without_host_fails() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        let err = exec
            .execute_script(&editor, "<test>", "call serverstart()")
            .unwrap_err();
        assert_eq!(error_code(&err), "E900");
    }

    #[test]
    fn serverstop_removes_and_rebumps_v_servername() {
        let (editor, mut exec, _state) = harness();
        exec.execute_script(&editor, "<test>", "call serverstart('/tmp/Xtest-a')")
            .unwrap();
        exec.execute_script(&editor, "<test>", "call serverstart('/tmp/Xtest-b')")
            .unwrap();
        assert_eq!(servername(&editor).as_deref(), Some("/tmp/Xtest-a"));
        exec.execute_script(&editor, "<test>", "call serverstop('/tmp/Xtest-a')")
            .unwrap();
        assert_eq!(servername(&editor).as_deref(), Some("/tmp/Xtest-b"));
        exec.execute_script(&editor, "<test>", "call serverstop('/tmp/Xtest-b')")
            .unwrap();
        assert_eq!(servername(&editor).as_deref(), Some(""));
    }

    #[test]
    fn serverstop_returns_zero_for_unknown_and_empty() {
        let (editor, mut exec, _state) = harness();
        exec.execute_script(&editor, "<test>", "let g:r1 = serverstop('')")
            .unwrap();
        exec.execute_script(&editor, "<test>", "let g:r2 = serverstop('bogus')")
            .unwrap();
        assert_eq!(global_number(exec.scope(), "r1"), Some(0));
        assert_eq!(global_number(exec.scope(), "r2"), Some(0));
    }

    #[test]
    fn serverstop_rejects_non_string_argument() {
        let (editor, mut exec, _state) = harness();
        let err = exec
            .execute_script(&editor, "<test>", "call serverstop(1)")
            .unwrap_err();
        assert_eq!(error_code(&err), "E474");
    }

    #[test]
    fn serverlist_returns_live_addresses() {
        let (editor, mut exec, _state) = harness();
        exec.execute_script(&editor, "<test>", "call serverstart('Xtest-a')")
            .unwrap();
        exec.execute_script(&editor, "<test>", "call serverstart('Xtest-b')")
            .unwrap();
        exec.execute_script(&editor, "<test>", "let g:list = serverlist()")
            .unwrap();
        match global(exec.scope(), "list") {
            Some(Typval::List(items)) => {
                let strings: Vec<String> = items
                    .borrow()
                    .items
                    .iter()
                    .filter_map(|item| match item {
                        Typval::String(s) => Some(s.to_string_lossy().into_owned()),
                        _ => None,
                    })
                    .collect();
                // Bare names resolve into $XDG_RUNTIME_DIR name.pid.count
                // addresses (server_address_new, server.c:116-136); with /
                // or : the address is verbatim (server.c:179-181).
                assert_eq!(strings.len(), 2, "both listeners listed");
                for (index, name) in ["Xtest-a", "Xtest-b"].into_iter().enumerate() {
                    let address = &strings[index];
                    assert!(
                        address.contains(name),
                        "{address} must carry the requested name"
                    );
                    assert!(
                        address.contains(&std::process::id().to_string()),
                        "{address} must carry the pid"
                    );
                }
            }
            other => panic!("expected list, got {other:?}"),
        }
    }

    #[test]
    fn serverlist_empty_when_nothing_listens() {
        let (editor, mut exec, _state) = harness();
        exec.execute_script(&editor, "<test>", "let g:list = serverlist()")
            .unwrap();
        match global(exec.scope(), "list") {
            Some(Typval::List(items)) => assert!(items.borrow().items.is_empty()),
            other => panic!("expected list, got {other:?}"),
        }
    }
}
