# egui_term rev 31bbc7a, patched for KXS Watcher

A copy of https://github.com/Harzu/egui_term at `31bbc7ab8503c9518fcee5717cfa29011e59f451` (MIT),
without the upstream workspace and examples. The change is marked `KXS` in the source:

- `src/view.rs` (`process_input`): keys go to the terminal that has keyboard focus wherever the
  pointer is; upstream also required the pointer over the terminal, so typing right after clicking
  a "Terminal" button (pointer still on it), or with the mouse moved away, was lost. Mouse input
  (wheel, clicks, selection, hover) still needs the pointer over the terminal.

Used through `egui_term = { path = "vendor/egui_term" }` in the root `Cargo.toml`. To upgrade,
re-apply this change to the new version or drop the copy.
