# Oxvim domain context

## Editor directory state

**Effective directory** is the process working directory that editor commands and plugins observe for the current window. Oxvim treats it as a cache of editor-owned state, not as the owner of directory history.

**Global directory** is the session-level directory used by windows without a window-local override. The editor owns its previous-directory slot for `:cd -`.

**Window-local directory** is an absolute directory override owned by one window. The same window owns the previous-directory slot used by `:lcd -`. Replacing the window's buffer does not replace either slot.

Every explicit or implicit current-window transition reapplies the destination window's effective directory. A failed directory change commits no process or editor state.