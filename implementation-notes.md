# Implementation notes

## Deviations

- Removed the pending-error design after upstream source (`window.c:5365`) showed that automatic `os_chdir` failure is silent. The editor does not queue `E344` on lifecycle transitions and leaves the prior process working directory in place.
- Treated `set_window_buffer` as an effective working directory transition. Buffer replacement reapplies the active window directory.
- Added direct `Editor::close_tabpage` test coverage because the `:tabclose` Ex command wrapper masked the missing lifecycle hook by calling `set_current_tabpage`.
- Made new tabpages and window splits inherit the source window local and previous directory state.

## Discovered edge cases

- Neovim has distinct tabpage- and buffer-local directory slots. The current Oxvim command surface exposes global and window scopes, so this change does not misrepresent either extra scope as window state.
- A relative `:lcd` target must be read back from the process after a successful change. Storing the argument would make later window switches resolve it against the wrong directory.
- Temporary window contexts must persist directory changes on the target window while restoring the caller's effective process directory.

## Related documentation

- [ADR-001: Own Working Directory State in the Editor Model](docs/decisions/ADR-001-editor-owned-working-directory-state.md)
- [Architecture Solution: Working Directory State Owner](docs/solutions/architecture/current-directory-state-owner.md)