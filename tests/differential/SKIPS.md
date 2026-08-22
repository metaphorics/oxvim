# Sanctioned differential divergences

Each entry must name one replay session and give a one-line behavioral justification. Add entries only through `replay --bless --reason` after inspecting the printed semantic diff.
- replay/sessions/core.yaml [sha256:2cbe1979dc517eeeacdc2e86d85a858af0b76d3f759859e2452b2e81cfe5960a] — Oxvim's API metadata surface is incomplete; all subsequent core smoke responses match upstream.
- replay/sessions/channels.yaml [sha256:8e3470b9a95be89b3855696864f8c599e0f0db30816b6da97093d9e57a58df5b] — Oxvim does not yet connect terminal job state, so nvim_open_term used for the chan_send echo probe returns NotImplemented; unknown-method error_event matches upstream.
- replay/sessions/options.yaml [sha256:550b667f58ac4e2854ab1fea2fe2c9f47033a64b5b7970a5243ea362b4bac5f3] — Upstream returns the assigned option value while Oxvim follows its void metadata and returns nil; both subsequent reads contain the assigned values.
- replay/sessions/ui_attach.yaml [sha256:904cf07bf64439f0d3cdb6e11ec97fc5bafaaf08c6f4238e179565f7c8980001] — Oxvim replies to ui_attach before its initial redraw and emits a smaller client-owned startup event set; both initial redraw batches terminate with flush.
