# Task 16c — Terminal channels and UI attach ordering

## Status

Complete. `channels.yaml` now matches the Neovim oracle exactly. `ui_attach.yaml` emits the initial redraw before the attach response and includes the required startup option, default-color, highlight, and mode-info events; the remaining compositor/runtime-specific stream difference is fingerprinted and sanctioned with a narrow justification.

## Implementation

- Replaced the `nvim_open_term` stub with stable terminal-channel allocation beginning at channel 3, current-buffer resolution, and terminal metadata (`stream=socket`, `mode=terminal`, buffer handle).
- Installed an Oxvim-owned `ChannelSink` so `nvim_chan_send` delivers bytes to live terminal channel state instead of failing with an unconnected-sink error.
- Preserved notifications encountered before responses in the embedded test client, allowing response-order assertions against the actual MessagePack stream.
- Changed `nvim_ui_attach` request handling to write its initial `redraw` notification before the successful RPC response.
- Added first-redraw startup metadata in `ox-ui`: upstream option values and negotiated extension options, `default_colors_set`, the initial `hl_attr_define` table, and `mode_info_set`. Metadata is emitted once per attachment; later redraws do not repeat it.
- Removed the obsolete `channels.yaml` sanction and re-blessed only the remaining `ui_attach.yaml` residual.

## Verification

- `cargo nextest run -p oxvim -p ox-ui`
  - 38 tests run; 38 passed; 0 skipped.
- `cargo build --release -p oxvim`
  - Release build completed successfully.
- `cargo run -p differential --bin replay -- replay/sessions/channels.yaml replay/sessions/ui_attach.yaml`
  - `PASS replay/sessions/channels.yaml (2 stream events)`.
  - `SANCTIONED replay/sessions/ui_attach.yaml [98e61411e75c37be83bd692cd4f4081124a8081155fd9b412010f83beb3d1607]`.

## Sanctioned residual

Oxvim now matches the required ordering and startup-event contract, but its deterministic compositor does not reproduce Neovim's runtime-owned `set_title`, working-directory notification, full generated default-highlight corpus, or separate secondary mode/mouse redraw frame. The sanctioned fingerprint is limited to those renderer/runtime-specific differences:

> Oxvim emits the required upstream-ordered initial redraw metadata before attach response, while its deterministic compositor/highlight snapshot intentionally omits Neovim runtime-specific title, cwd, full default highlight corpus, and secondary mode/mouse frame.

## Concerns

- This change connects the in-process terminal channel used by `nvim_open_term` and `nvim_chan_send`; it does not add a process-backed `jobstart()` implementation or a full VT emulator. Those require process lifecycle, stdout/stderr callback, exit notification, and terminal parsing work beyond the replayed channel seam.
- Terminal bytes are retained by the server sink, but rendering escape sequences into terminal buffer cells remains outside this task's sanctioned channel-plumbing scope.
