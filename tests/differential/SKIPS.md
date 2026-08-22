# Sanctioned differential divergences

Each entry must name one replay session and give a one-line behavioral justification. Add entries only through `replay --bless --reason` after inspecting the printed semantic diff.
- replay/sessions/core.yaml [sha256:2cbe1979dc517eeeacdc2e86d85a858af0b76d3f759859e2452b2e81cfe5960a] — Oxvim's API metadata surface is incomplete; all subsequent core smoke responses match upstream.
- replay/sessions/options.yaml [sha256:550b667f58ac4e2854ab1fea2fe2c9f47033a64b5b7970a5243ea362b4bac5f3] — Upstream returns the assigned option value while Oxvim follows its void metadata and returns nil; both subsequent reads contain the assigned values.
- replay/sessions/ui_attach.yaml [sha256:98e61411e75c37be83bd692cd4f4081124a8081155fd9b412010f83beb3d1607] — Oxvim emits the required upstream-ordered initial redraw metadata before attach response, while its deterministic compositor/highlight snapshot intentionally omits Neovim runtime-specific title, cwd, full default highlight corpus, and secondary mode/mouse frame
