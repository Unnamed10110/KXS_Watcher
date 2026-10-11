# egui_term rev 31bbc7a, patched for KXS Watcher

A copy of https://github.com/Harzu/egui_term at `31bbc7ab8503c9518fcee5717cfa29011e59f451` (MIT),
without the upstream workspace and examples. The change is marked `KXS` in the source:

- `src/view.rs` (`process_input`): keys go to the terminal that has keyboard focus wherever the
  pointer is; upstream also required the pointer over the terminal, so typing right after clicking
  a "Terminal" button (pointer still on it), or with the mouse moved away, was lost. Mouse input
  (wheel, clicks, selection, hover) still needs the pointer over the terminal.

Used through `egui_term = { path = "vendor/egui_term" }` in the root `Cargo.toml`. To upgrade,
re-apply this change to the new version or drop the copy.

## Performance (Phase 3.3)

Measured with a PTY holding 12,000 lines of output (`bench_terminal_frame_full_scrollback` in
`src/tabs/term.rs`): one idle frame of a terminal tab cost 14.0 ms, 9.9 ms of it `sync()` cloning the
whole grid (scrollback included) on every frame.

- `src/backend/mod.rs`: `sync()` copies only the cells on screen (`RenderableContent::cells`,
  `display_offset`, `cursor_point` replace `grid`) and only when `dirty`: set by the thread that
  forwards the PTY's events and by `process_command` (a resize counts only when the size changed).
  `generation` counts the copies. `open_link` reads the link from the terminal itself, since the copy
  has no scrollback.
- `src/backend/settings.rs`: `BackendSettings::scrollback` (default 10,000 as alacritty; the app passes
  its Settings value, 5,000), so a full history costs half the memory.
- `src/view.rs` (`show`): plain ASCII cells of one color along a row are painted as one galley
  (`TextRun`, glyph pitch kept with `extra_letter_spacing`), not one `Shape::text` per cell: ~100 shapes
  a screen instead of ~6,000. Wide chars, non-ASCII chars and the cursor cell are still drawn one by one.

Idle frame: 14.0 ms → 0.3 ms.
