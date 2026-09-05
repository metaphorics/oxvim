//! Focused regression tests for Task 78 filesystem findings.
//!
//! These live as an integration test because the workspace's `#[cfg(test)]`
//! modules currently carry unrelated compile errors; this file compiles only
//! the release library and exercises the public script surface.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use ox_editor::{Editor, ExExecutor, ExecError, TestEditorAccess, VimExceptionKind};
use ox_eval::ScopeKind;
use ox_types::Typval;

struct TempRoot(PathBuf);

impl TempRoot {
    #[expect(
        clippy::unwrap_used,
        reason = "temporary-directory setup must stop when the host filesystem fails"
    )]
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ox-editor-task78-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn read_flag(executor: &ExExecutor, name: &[u8]) -> Option<Typval> {
    executor
        .scope()
        .get_scoped(ScopeKind::Global, name, 0)
        .cloned()
        .ok()
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "the regression test must stop when script execution fails"
)]
fn mkdir_plain_p_survives_function_return() {
    let root = TempRoot::new("mkdir-plain-p");
    let base = root.0.display().to_string();

    let editor = TestEditorAccess::new(Editor::new());
    let mut executor = ExExecutor::new();
    executor
        .execute_script(
            &editor,
            "mkdir-plain-p.vim",
            &format!(
                "func MakeDir(name)\n\
                 call mkdir(a:name, 'p')\n\
                 endfunc\n\
                 call MakeDir('{base}/Xplain')\n\
                 let g:after = isdirectory('{base}/Xplain')"
            ),
        )
        .unwrap();

    assert!(root.0.join("Xplain").is_dir());
    assert_eq!(read_flag(&executor, b"after"), Some(Typval::Number(1)));
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "the regression test must stop when script execution fails"
)]
fn defer_delete_honors_d_and_rf_flags() {
    let root = TempRoot::new("defer-delete");
    let base = root.0.display().to_string();

    let editor = TestEditorAccess::new(Editor::new());
    let mut executor = ExExecutor::new();
    executor
        .execute_script(
            &editor,
            "defer-delete.vim",
            &format!(
                "func DeferFile(name)\n\
                 call writefile(['x'], a:name)\n\
                 defer delete(a:name)\n\
                 endfunc\n\
                 func DeferDir(name)\n\
                 call mkdir(a:name, 'p')\n\
                 defer delete(a:name, 'd')\n\
                 endfunc\n\
                 func DeferRec(name)\n\
                 call mkdir(a:name, 'p')\n\
                 call writefile(['x'], a:name .. '/file')\n\
                 defer delete(a:name, 'rf')\n\
                 endfunc\n\
                 func Suite()\n\
                 call DeferFile('{base}/file')\n\
                 let g:file_gone = !filereadable('{base}/file')\n\
                 call DeferDir('{base}/dir')\n\
                 let g:dir_gone = !isdirectory('{base}/dir')\n\
                 call DeferRec('{base}/rec')\n\
                 let g:rec_gone = !isdirectory('{base}/rec') && !filereadable('{base}/rec/file')\n\
                 endfunc\n\
                 call Suite()"
            ),
        )
        .unwrap();

    assert_eq!(read_flag(&executor, b"file_gone"), Some(Typval::Number(1)));
    assert_eq!(read_flag(&executor, b"dir_gone"), Some(Typval::Number(1)));
    assert_eq!(read_flag(&executor, b"rec_gone"), Some(Typval::Number(1)));
}

#[test]
#[expect(
    clippy::panic,
    clippy::unwrap_used,
    reason = "the regression test asserts the exact invalid-flag error path"
)]
fn defer_delete_rejects_invalid_flags() {
    let root = TempRoot::new("defer-delete-invalid");
    let base = root.0.display().to_string();
    let target = root.0.join("target");
    fs::write(&target, b"x").unwrap();

    let editor = TestEditorAccess::new(Editor::new());
    let mut executor = ExExecutor::new();
    let result = executor.execute_script(
        &editor,
        "defer-delete-flags.vim",
        &format!(
            "func Bad(name)\n\
             defer delete(a:name, 'x')\n\
             endfunc\n\
             call Bad('{base}/target')"
        ),
    );
    let error = result.unwrap_err();
    let ExecError::Vim(exception) = error else {
        panic!("expected a Vim error for invalid defer delete flags, got {error:?}")
    };
    assert_eq!(exception.kind, VimExceptionKind::Error("E15".to_owned()));
    assert!(
        target.is_file(),
        "file must not be deleted when defer delete is rejected"
    );
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "the regression test must stop when setup or error extraction fails"
)]
fn defer_delete_rejects_extra_argument_and_leaves_file() {
    let root = TempRoot::new("defer-delete-arity");
    let base = root.0.display().to_string();
    let target = root.0.join("target");
    fs::write(&target, b"x").unwrap();

    let editor = TestEditorAccess::new(Editor::new());
    let mut executor = ExExecutor::new();
    let result = executor.execute_script(
        &editor,
        "defer-delete-arity.vim",
        &format!(
            "func Bad()\n\
             defer delete('{base}/target', '', 'x')\n\
             endfunc\n\
             call Bad()"
        ),
    );
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("E118"),
        "expected E118 for too many defer delete arguments, got {err}"
    );
    assert!(
        target.is_file(),
        "file must not be deleted when defer delete is rejected"
    );
}
