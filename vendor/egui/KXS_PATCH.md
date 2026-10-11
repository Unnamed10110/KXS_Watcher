# egui 0.35.0, patched for KXS Watcher

A copy of egui 0.35.0 from crates.io (MIT OR Apache-2.0) with editor-style text selection.
Every change is marked `KXS` in the source:

- `src/input_state/mod.rs`: `PointerState::press_count()` tells a single, double or triple *press*
  (egui only counts clicks, on release).
- `src/text_selection/text_cursor_state.rs`: a double-click selects a whole token: letters, digits
  and `._-:/=@%+~`, so IPs, Kubernetes names, image references, paths and `key=value` labels select
  in one go (punctuation at the ends is left out). The selection starts on the press, and dragging
  on extends it by whole words; a triple press selects the line.
- `src/text_selection/label_text_selection.rs`: the same word-wise drag for labels.
- Dragging selected text out of the window (the app does the OS drag, `src/dragout.rs`):
  - `src/context.rs`: `Context::kxs_request_drag_out(text)` / `kxs_take_drag_out()`.
  - `src/text_selection/label_text_selection.rs`: a single press inside the selected text of a
    label keeps the selection (`drag_armed`) instead of starting a new one; moving on asks for the
    drag with the selected text, letting go without moving collapses the selection.
    `LabelSelectionState::selected_text()` gives the current selection as a string (egui only built
    it on a Copy event). The press frame no longer runs the drag-to-select step (the press decides).
  - `src/widgets/text_edit/builder.rs`: the same for a focused `TextEdit` (egui's own TODO "drag
    selected text"); the state lives in `ui.data` under `id.with("kxs-drag-armed")`.

Used through `[patch.crates-io]` in the root `Cargo.toml`. To upgrade egui, re-apply these
changes to the new version or drop the patch.
