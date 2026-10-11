# Changelog

Versions follow `Cargo.toml` (the single source of truth); a release is the git tag `v<version>` and
the MSI built from it. Newest first.

## 2.4.0 (unreleased)

### Added
- Logs: selection implemented by the app (drag past the edge to keep selecting, double/triple click,
  Shift+click, Shift+arrows/Page/Home/End, Ctrl+A/C), a followed log holds still while text is selected.
- Drag selected text (logs, details panel, YAML) out of the window into another program.
- Logs: Page Up/Down and arrow scrolling, ⬆ Start / ⬇ End buttons, "N new lines" pill, timestamps in
  local time (`2026-10-06 07:45:29.265`), "Logs by date" dialog, Download all.
- Find (Ctrl+F) in the details panel highlights every match, scrolls to the current one inside long
  values and also covers the secret key editor.
- Per-area font sizes, Lens-style filters (`a*|b*`), per-cluster colors, session restore of tabs,
  sizes and dock.
- Settings › Terminal: scrollback lines of the terminals opened from then on (default 5,000).
- Pod details: each container's environment variables (literal values; values from a Secret or a
  ConfigMap link to the object, never read), also found by the details search.
- Copy as kubectl: `get -o yaml`, `describe`, `logs -f` and `exec -it` for an object, naming the
  kubeconfig file and context (row menu and details bar); several rows give one line each.
- Lists by keyboard (pointer over the list): Up/Down, Page Up/Down, Home/End move the selection (Shift
  extends it), Enter opens the row in a tab, Delete asks to delete the selection.
- Export the rows shown, with the columns shown, as CSV or JSON (list toolbar › Export).

### Changed
- Faster and lighter (measured, `docs/PERF.md`): about 60 MB of memory with a cluster open (was ~100), the
  cluster ready in 0.7 s (was 1.8 s), the details panel, YAML tab and terminals no longer redo their work on
  every frame (a terminal tab with a full scrollback took 14 ms per frame, now 0.2 ms), lists of 50,000 rows
  rebuild up to 10x faster and at most four times a second, log search is 2 – 4x faster, hidden pages and
  a minimized window no longer wake the app, a 22.6 MB exe (was 23.1).
- YAML colors come from a small built-in tokenizer instead of syntect: the YAML editor no longer lags
  while typing, big objects are colored too; the palette changed a little.
- Long-running tasks that panic no longer take the window down at the next lock; unwraps outside tests
  are a clippy warning.
- New measuring tools (frame HUD with repaint causes, section timers, startup trace, benchmarks) and a
  recorded performance baseline in `docs/PERF.md`; clippy-clean code base (`-D warnings` in CI).
- Code structure: `main.rs`, `tabs.rs` and `cluster.rs` split into modules (largest file now `details.rs`),
  the log view's selection and layout moved out of the 370-line `lines_ui`; background results that never
  arrive (a panicked task) now end with an error instead of an endless spinner; more tests (56 → 69, plus 3
  benchmarks): fake-API tests of the watch and the log stream, list selection and sorting, settings files.
- The details panel is a full-height column; the logs dock narrows to its left.
- The patched egui (`vendor/egui`) is now part of the repository, so a fresh clone builds.
- Releases: the tag must equal the `Cargo.toml` version; the MSI comes with a `.sha256` file;
  CI (GitHub Actions) builds and tests every push.

### Fixed
- Logs no longer show duplicated lines after Reload / Start / End, and End no longer blanks the view.
- Typing in terminals and text boxes no longer beeps; Alt codes (`Alt+124` for `|`) work.
