# Performance baseline and how to measure

The rule for every change: re-run these measurements and do not regress any of them by more than ~5%
(see `src/prof.rs`). All numbers are from a release build on the developer machine; they are for
comparing before/after on the same machine, not absolute targets.

## Tools

| What | How |
|---|---|
| Frame HUD (bottom right): frame CPU time (last / avg / p95), frames total, in the last second and the last 60 s, private memory, **what asked for each frame** (file:line, counted over 60 s) and the time in the `prof!` sections | `KXS_HUD=1`, or Ctrl+Shift+F12 in a running app |
| Section timers | `crate::prof!("name");` at the top of a function or block; shown in the HUD (n, total ms, max ms per second) |
| Allocations per frame in the HUD | `cargo build --release --features profile` (counts every allocation) |
| Startup timeline | `KXS_TRACE=1`: appends `[+ms] label` lines to `%TEMP%\kxs-trace.log` |
| Micro benchmarks | `cargo test --release -- --ignored --nocapture --test-threads=1 bench_` |

Sections instrumented so far: `List::refresh`, `Details::ui`, `details yaml`, `LogTab::lines_ui`, `nav`,
`kubeconfig::stamp`.

## Baseline (2026-10-11, version 2.4.0, before any optimization)

### Binary
- `kxs-watcher.exe` (release): **23.12 MB**.

### Startup (`KXS_TRACE=1`, restored session: Secrets page, details panel and one log tab, QA cluster)
| Milestone | ms since `main` |
|---|---|
| `main` | 1.5 |
| `App::new` called (window + GL context created) | 280 – 410 |
| `App::new` done | +20 – 50 after the call |
| first frame | 330 – 460 |
| `connect`: API version answered | ~740 |
| `connect`: discovery done | ~970 |
| cluster ready (CRD list, pages restored) | ~1,790 |
| first list synced | ~1,850 |

### Idle (nothing touched for 60 s, HUD on)
| Situation | Frames / 60 s | Frame CPU (avg / p95) | Private memory |
|---|---|---|---|
| QA cluster connected, details panel (a Secret) + 1 log tab streaming | 59 | 2.5 / 3.3 ms | 107 MB |
| No cluster reachable (connect error page) | 87 | 0.33 / 1.5 ms | 53.5 MB |

Repaint causes while idle with the cluster (counted over 60 s): `main.rs:460` (the 3 s kubeconfig
poll re-armed every frame) 59×, `context.rs:525` 15×, `scroll_area.rs:1473/1345` 17× (scroll-bar
animation), `watch.rs:271` 2×.

Section times in that state (per second): `Details::ui` 4 calls, 6.9 ms total (**1.7 ms per frame for a
5-key Secret**, all of it cloning / re-serializing / re-highlighting each frame), `nav` 0.7 ms,
`LogTab::lines_ui` 0.2 ms, `List::refresh` 0.01 ms.

### Micro benchmarks (`bench_` tests, ms per run)
List `refresh` over **50,000** pods (7 columns, images, 40 namespaces), forced on every run
(= what the next frame pays after a watch event):

| Case | ms |
|---|---|
| default sort (namespace, name) | 48.3 |
| wildcard filter `service-0*\|node-3` | 117.8 |
| regex filter | 112.3 |
| sort by Restarts | 97.6 |
| sort by Age | 11.5 |
| sort by Images | **307.7** |
| sort by Namespace | 80.4 |

Log buffer, **100,000** JSON lines (~140 chars):

| Case | ms |
|---|---|
| insert all lines in order | 55.4 |
| find scan, plain matcher, 1% match | 40.1 |
| find scan, plain matcher, no match | 28.1 |
| find scan, regex (`err(or)? \d+`) | 18.7 |
| find scan, `hit()` then `ranges()` | 90.0 (slower: don't) |
| `lines_ui` frame, idle / streaming | 0.10 / 0.10 |
| `lines_ui` frame, Wrap on, streaming | 1.13 |
| `lines_ui` frame, **find open**, streaming (full rescan) | **45.4** |

What these say: a watch event on a 50k-row list costs a 50–300 ms frame; an open find on a busy log
costs ~45 ms per rescan, and a regex scan is cheaper than the byte-by-byte plain matcher (so build
the plain matcher as an ASCII case-insensitive regex instead of adding a `hit()` pre-check);
`Details::ui` pays ~1.7 ms every frame for a small Secret before caching.

## Targets for Phase 3 (from the audit; keep them honest by re-measuring)
- `Details::ui` per frame: from 1.7 ms to < 0.1 ms once cached by `rv`.
- `List::refresh` at 50k rows: throttled to ~4/s and sorted outside the lock; sort by Images < 100 ms.
- Idle frames/min with the cluster: from 59 to a handful (the 3 s poll in a background task, no
  repaints for hidden pages / minimized window).
- Find on a 100k-line log: < 20 ms per rescan.
- No regression in private memory (107 MB with a cluster, 53.5 MB without) or startup.

## Phase 3 results (each item against the baseline above; same machine, release profile)

### 3.1 Details view: cached per version of the object (`src/details.rs`, `src/find.rs`)
The object is an `Arc<Value>` (no deep clone per frame); the YAML text, its colors and its laid-out
galley, and the decoded Secret / ConfigMap values are built once per `resourceVersion` (`Cache`),
`find::job` keeps the galley while nothing matches. `bench_details_frame_1mb` (40 keys of 25 KB):

| Case (ms per frame) | before | after |
|---|---|---|
| ConfigMap 1 MB, panel | 0.6 – 1.1 | 0.42 |
| ConfigMap 14 KB, panel | – | 0.06 |
| Secret 1 MB, panel (base64 decode of every value was per frame) | 0.6 – 1.2 | 0.43 |
| ConfigMap 1 MB, YAML tab | 3.5 – 4.3 | **0.02** |
| Secret 1 MB, YAML tab | 2.4 – 5.4 | **0.02** |
| ConfigMap 1 MB, panel with find open | 7.7 – 9.2 | 0.66 |
| Secret 1 MB, panel with find open | 7.5 – 9.2 | 0.47 |
| Building the YAML (text + colors) of a 144 KB object, once per version | 1,170 | **1.05** |
| Building it for 14 KB | 133 | 0.29 |

What is left in the panel at 1 MB is laying out and painting 40 labels of 25 KB each (0.4 ms; 0.06 ms at
14 KB). The first frame of a 1 MB YAML tab still lays the text out once (about 210 ms, egui's cost for a
1 MB galley; later frames reuse it). In the running app (HUD, a Pod in the details panel) `Details::ui`
takes 0.1 – 0.3 ms per frame; the baseline measured 1.7 ms for a 5-key Secret.

Finding that changed the plan: the syntax colors (syntect through `egui_extras::highlight`) cost
**~9 ms per KB** (14 KB: 134 ms, 28 KB: 265 ms, 144 KB: 1.2 s) and egui runs it while holding the
context memory lock, so moving it to a thread would have frozen the UI just the same. It was paid again
every time the YAML section was shown, and on every keystroke in the YAML editor. 3.8 replaced syntect
by a tokenizer (`src/yamlhl.rs`, 730 KB in 29 ms), so the cost, the cutoff that was going to be needed
and the editor's 200 KB cutoff are gone.

### 3.2 Quick wins
- `ui_kit::spinner` (14 steps a second, `request_repaint_after`) replaces the 11 `ui.spinner()`; egui's
  own asks for a repaint on every frame it is visible, i.e. 60 frames a second while anything loads
  (test: `a_spinner_wakes_the_ui_14_times_a_second_not_every_frame`).
- `kube` feature `gzip`: the API server compresses big LIST answers (3 small crates added).
- Search page: the object count is computed once per load (was a `HashSet` over every key each frame)
  and the scan for a query runs on a blocking thread that is stopped when the query changes
  (`Scan`; the table keeps showing the previous hits until the new ones are in).

### 3.3 Terminal (`vendor/egui_term`, see its `KXS_PATCH.md`)
`bench_terminal_frame_full_scrollback`: a real PTY (`cmd.exe`) with 12,000 lines of output behind the
screen (not in the baseline above: the terminal was never measured; it was the worst offender found
while looking).

| Case (ms per frame) | before | after |
|---|---|---|
| terminal tab, nothing changing | **14.0** | **0.20** |
| `sync()` alone (copy of the screen for drawing) | 9.9 | 0.00 (nothing to copy) |

`sync()` cloned the whole alacritty grid, scrollback included, on every frame; it now copies the visible
cells only and only when the terminal changed (`dirty`, set by the PTY event thread and by commands).
Plain ASCII of one color along a row is painted as one galley instead of one per cell (~100 shapes a
screen instead of ~6,000), glyphs kept on the cell grid (tests: `text_is_drawn_in_runs_on_the_cell_grid`,
`the_screen_is_copied_only_when_something_changed`). Scrollback is a setting now (Terminal page,
default 5,000 lines instead of alacritty's 10,000; each kept line is `columns` cells of memory).
Checked in the running app (local PowerShell tab: colors, prompt, cursor identical).

### 3.4 Lists (`src/list.rs`, `src/cluster/*`)
`bench_list_refresh_50k` (the cost of one rebuild of the view; the app now does it at most 4 times a
second while the data changes, and filters, sorts and clones the rows outside the lock):

| Case (ms) | before | after |
|---|---|---|
| default sort (namespace, name) | 48.3 | 35.1 |
| wildcard filter `service-0*\|node-3` | 117.8 | 66.6 |
| regex filter | 112.3 | 73.7 |
| sort by Restarts | 97.6 | 51.5 |
| sort by Age | 11.5 | 1.8 |
| sort by Images | **307.7** | **27.0** |
| sort by Namespace | 80.4 | 40.1 |

The expensive sorts build their key once per row (`sort_by_cached_key`: Images joined and compared
strings, CPU / Memory looked up a `(String, String)` key, on every comparison). The metrics' timestamp is
part of the view's key only when sorted by CPU or Memory (every metrics poll rebuilt every list before).
`status_counts`, the events grouping, the workloads' breakdown and `selected()` are kept until the data or
the selection changes (they ran over every row on every frame); the Ctrl+K scan of cells builds no
`String` for plain cells. Tests: `a_changing_list_is_rebuilt_a_few_times_a_second`,
`what_is_derived_from_the_rows_is_kept_until_they_change`, `sorts_by_images_and_by_usage`.

### 3.5 Repaint hygiene (`src/repaint.rs`, `src/kubeconfig.rs`)
- `repaint::begin_frame` notes a minimized window; `repaint::after` (spinner, toasts, metrics and count
  loops) does nothing then.
- `Shown`: whatever draws a list or a log tab marks it every frame; the watch and the log stream wake the
  window at full pace only while the mark is fresh, and a list nobody draws wakes it every 2 s at most
  (sidebar counts stay current). A hidden log tab never wakes it.
- The kubeconfig check (a 3 s timer that re-armed itself on every frame, and file I/O on the UI thread)
  is a background task that wakes the UI only when a file changed (`kubeconfig::Watch`).
- With no search open a log no longer asks for a frame 260 ms later for a scan that has nothing to scan.
- Not done: starting the lists of hidden pages lazily. They no longer wake the window, and the saved
  filter, sort and open row would have to wait until first draw; revisit if a session with many pages
  shows a startup cost.

Idle, 60 s, the QA cluster, details panel and one followed log tab (HUD, final build): **38 frames**
(baseline 59), the causes being the log stream itself (28), scroll animation (25) and the tasks (5);
the 3 s kubeconfig wake-up (59 in the baseline) and the 260 ms scan wake-up are gone.

### 3.6 Logs (`src/tabs/logs/*`, `src/find.rs`)
`bench_log_*`, 100,000 lines:

| Case (ms) | before | after |
|---|---|---|
| insert all lines in order | 55.4 | 34.0 |
| find scan, plain, 1% match (every line has `error` as a key here) | 40.1 | **17.0** |
| find scan, plain, no match | 28.1 | **6.6** |
| find scan, regex | 18.7 | 11.3 |
| `lines_ui` frame, find open, streaming (full rescan) | 45.4 | **16.6** |
| `lines_ui` frame, idle | 0.10 | 0.06 |
| `lines_ui` frame, Wrap on, streaming | 1.13 | 0.57 |

- The plain case-insensitive matcher is an ASCII-folding regex (`unicode(false)`): scans are 2 – 4 times
  faster (the regex engine searches at memory speed); `Matcher::count` counts without building ranges.
- Stream tasks send lines through a channel (`Feed` / `Inbox`); the tab drains it once a frame under one
  lock. Before, every line locked the buffer, so a task blocked a tokio worker while the UI held the lock
  for a 25 ms scan; the window is woken for the first line after a drain, not for each.
- The buffer keeps at most `MAX_LINES` lines and `MAX_BYTES` (64 MB): 100,000 lines of 10 KB were a gigabyte.
- Not done: incremental scan and wrap rows (the numbers above leave no frame worth the complexity),
  and timestamp / level pre-formatting (a few microseconds per frame for the rows on screen).

### 3.7 Startup and network (`src/cluster.rs`, `src/watch.rs`, `src/main.rs`)
`connect` asks for the server version, the API discovery and the CRD names at the same time (it was one
after the other), the CRD list as a names-only Table (`includeObject=None`; the metadata of a CRD-heavy
cluster is megabytes of annotations). `helm` is probed on a thread and the UI never waits for it. The
window icon is 128 px. `KXS_TRACE=1`, QA cluster, restored session, ms since `main` (3 runs):

| Milestone | before | after |
|---|---|---|
| cluster ready | ~1,790 | **670 – 820** |
| first list synced | ~1,850 | **730 – 970** |

Not done: fonts and kubeconfig discovery on threads overlapping the GL context (`App::new` takes 5 – 50 ms).

### 3.8 Binary and memory
Measured in a copy of the tree, same sources, each build kept or dropped on its numbers (exe bytes; private
memory of the same restored session after 20 s and 60 s, two runs each):

| Build | exe | private memory | build time |
|---|---|---|---|
| 2.4.0 MSI exe, before Phase 3 | 24.21 MB | 101 MB | – |
| current sources, `lto = "thin"` (control) | 23.69 MB | 98 – 104 MB | 15 min |
| + `opt-level = "s"` for the crates that talk to the cluster and the OS | **22.61 MB** | **59 – 66 MB** | 11 min |
| `lto = "fat"` | 22.62 MB | 61 – 65 MB | 18 min |
| `mimalloc` as the allocator | 23.53 MB | 155 MB | 11 min |

Kept: `opt-level = "s"` for 20 cold crates (`Cargo.toml`): final exe **22.59 MB**, idle private memory
**60 – 66 MB** (HUD: 60.9 MB), frame times unchanged. Dropped: `mimalloc` (+55 MB: it keeps what it frees)
and fat LTO (the same result, a slower build). I do not know why the thin-LTO build holds ~35 MB more at
idle than the other two (they were repeated in both orders and the effect did not move); the cold-crate
setting is kept because it measures better and costs nothing, not because it is understood. The exe is
smaller than both pre-Phase-3 figures (23.12 MB in the baseline section, 24.21 MB for the 2.4.0 MSI's exe)
although it gained gzip and the new features.

syntect is gone (`egui_extras` without the `syntect` feature: 17 crates fewer in `Cargo.lock`, 3 added for gzip) for the
tokenizer in `src/yamlhl.rs`; the colors are a Catppuccin-like palette, not syntect's. The HUD shows the
font atlas size (2.0 MB here). Idle private memory is the number to watch: with a cluster, details panel
and a log tab it is ~61 MB; the baseline of 107 MB was a thin-LTO build.

## Phase 4 (consolidation and lints)
- Tabs: the three tab rows share `ui_kit::Rename`, `move_menu`, `close_menu` and `keep_only` (about 100
  lines fewer, one rename flow, tested). `details::chip` stays as it is: it wraps and takes part in find,
  which `ui_kit::chip` does not, so they are not the same widget.
- One status dot (`ui_kit::dot`); all case-insensitive text matching goes through `find::Matcher`.
- `keys.rs` uses the `windows` crate's `MSG`, `WM_*` and `VK_*` instead of its own declarations.
- A mutex is locked with `Lock::locked` (it recovers a mutex poisoned by a task that panicked: that task
  used to take the UI thread down at its next lock); clippy warns on `unwrap` / `expect` outside tests
  (`Cargo.toml` `[lints.clippy]`, `clippy.toml`), so CI fails on a new one.

## Exit check of Phase 3 (final release build, quiet machine)
| Target | Result |
|---|---|
| `Details::ui` per frame < 0.1 ms once cached | 0.06 ms (14 KB, bench); 0.1 – 0.3 ms in the app for a Pod |
| list refresh at 50k, Images sort < 100 ms | **27 ms**; every sort at least 1.4x faster |
| idle frames per minute from 59 to a handful | 38 with a live log tab (3 s wake-up gone) |
| find on a 100k-line log < 20 ms | **16.6 ms** per frame with find open and streaming (scan: 7 – 17 ms) |
| no regression in memory or startup | memory **60 MB** (was 100+); startup to cluster ready **0.7 s** (was 1.8 s) |
| terminal | 14 ms to 0.2 ms per idle frame with a full scrollback |
