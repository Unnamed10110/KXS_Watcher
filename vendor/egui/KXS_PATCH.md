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

Used through `[patch.crates-io]` in the root `Cargo.toml`. To upgrade egui, re-apply these three
changes to the new version or drop the patch.
