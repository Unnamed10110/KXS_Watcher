# KXS Watcher

Fast, lightweight native Kubernetes / k3s desktop GUI (a Lens alternative). Pure Rust:
egui (GPU-rendered, no webview), kube-rs, egui_dock, egui_term.

## Run

```
cargo run --release
```

Requires `kubectl` on PATH for terminals, pod shells, drain and port-forward.
`helm` on PATH enables the Helm Releases view.

## Features

- **Graphite design**: a top bar with the open clusters as colored pills, the Ctrl+K search box, warnings
  (click for the overview), theme and settings; a cluster sidebar with object counts; card-based
  overview, details and settings. Uses Segoe UI and Cascadia Mono, or Geist and JetBrains Mono when
  installed. The cluster list opens from "+" (and is the start screen).
- **Catalog**: contexts from `~/.kube/config`, `$KUBECONFIG` and any extra files/folders
  (Settings; `~/.kube` is scanned by default). Pick files or folders on disk with the native file
  dialog ("Add kubeconfig…" in the catalog, File menu, Settings). Pin, filter, open several clusters as tabs.
  Files edited, added or removed on disk (kubectl config, other tools) update the list within seconds.
- **Live lists** for every resource kind, including CRDs, built from the API server's Table
  output (the same columns as `kubectl get`, `Wide` = `-o wide`). Multi-namespace filter, search, sort,
  status colors, CPU/memory columns for pods and nodes (metrics-server). The filter (Ctrl+F) looks at names only,
  like Lens: `jpts*|ingenico*|as400*` keeps the names containing jpts, ingenico or as400 (`|` or, `*` anything);
  Ctrl+K finds text in every column;
  `.*` switches to a full regular expression. Alt codes (Alt+124 for `|`) type in every text box.
- **Multi-select**: checkboxes, Ctrl/Shift+click, Ctrl+A. Bulk open, merged logs, restart, scale,
  edit YAML, copy names, delete (one confirmation, one summary).
- **Object sub-tabs** inside each cluster tab (double-click a row). Details per kind link to
  everything they reference: owner, node, service account, config maps, secrets, volume claims,
  ingress backends, role bindings, HPA targets, event objects… plus related pods/ReplicaSets/Jobs.
  Each part (overview, conditions, references, every container, events…) is its own framed block
  with a colored header and a summary (`5 / 5 OK`, `running · 0 restarts`); key/value rows are striped.
- **Secret / ConfigMap editor**: edit, add and remove keys, Save (conflicts are detected).
- **Actions**: edit YAML, create from YAML (multi-document, server-side apply), delete, scale,
  rollout restart, cordon/uncordon, drain, CronJob trigger/suspend, port-forward.
- **Logs**: one pod or many (interleaved by time, pod name per line), ANSI and log-level colors,
  wrap, timestamps and pod name (on by default), previous container, ⬆ Start / ⬇ End (Ctrl+Home /
  Ctrl+End; scrolled up, "N new lines · Go to end" appears), Page Up / Page Down and the arrows scroll,
  text selects as in an editor (drag, and past an edge the log scrolls along; double/triple click for a
  word/line; Shift+click; Shift with the arrows, Page Up/Down and Home/End, with Ctrl for the whole log;
  Ctrl+A, Ctrl+C, Escape), a followed log holds still while something is selected (⬇ End goes live
  again), selected text (logs, details, YAML) can be dragged out of the window into another program, find matches marked on the scroll track, Ctrl+F find (plain or regex, only-matching), scrolling to the
  top loads earlier lines, dropped connections resume on their own. **📅 Logs by date** (a dialog) shows a
  date range, From and To: `2026-09-28 14:30`, `14:30` (today), `2h` / `30m` / `1d` ago, a UTC timestamp
  copied from a line, or Last 15 min / 1 h / 6 h / 24 h / Today / Yesterday; To empty keeps following.
  It can also find a text in those lines (only matching, regex) and download the range to a file.
  **Save…** writes the shown lines (filter and options applied) to a `.log` file; **⬇ Download all…**
  streams every line the pod still has (the previous container's with Previous, timestamps with
  Timestamps; several pods or containers one after the other), with its size as it goes and Cancel.
- **Ctrl+K** finds text in the current view (lists, details, YAML, logs): the current match solid amber,
  every other one amber and underlined; the list filter's matches are marked the same way, with a
  "N matches in M rows" count. Collapsed sections with
  matches open; hidden secret values are searched too and flagged without being revealed.
- **Search configs & secrets**: text (plain or regex) in every Config Map and Secret value, key and
  name, in all or the selected namespaces. Secret values stay masked unless shown; a click opens the
  object with the same text highlighted.
- **Themes**: Dark, Light, System, AMOLED in eight neon colors (cyan, red, green, purple, pink,
  blue, yellow, orange: pure black, only the accent lights up), Crimson, Forest, Ocean and Violet.
- **Font size by area** (Settings › Appearance): tabs, sidebar, lists and pages, details, logs, YAML
  editor and terminal each from 70 % to 160 % of the text size; the rest of the window keeps it.
- **Tabs**: Ctrl+T (or "+" after the page tabs) opens a new tab: type to pick a view or resource list.
  Drag tabs (clusters, pages, objects) to reorder them; double-click a cluster or page tab to rename it
  (right-click: rename, reset name, move left/right). Ctrl+W closes the tab (the object tab shown,
  else the page tab; over the dock, the dock tab under the pointer), Ctrl+Tab / Ctrl+Shift+Tab switch
  page tabs, Ctrl+1…8 go to that cluster and Ctrl+9 to the last one.
- **Remembers the UI**: window, theme and sizes, open clusters in order and the one shown, their names and
  colors, page and object tabs (names, order, Overview/Events/YAML), namespace filter, Wide, each list's
  filter/regex/sort/status and its open details row, the Search query, the sidebar, details panel
  and dock sizes, table column widths, and the dock's log tabs (with their options and date range,
  reopened once the cluster connects) and local terminals. Pod and node shells and YAML editors are
  not reopened (a node shell starts a privileged pod).
- **No Windows "ding" while typing**: keys that reach the window while nothing has the keyboard
  focus (after a native dialog, for one) used to beep; the window takes the focus back.
- **Image column** on pods and every workload (deployments, stateful/daemon/replica sets, jobs, cron jobs):
  `name:version` of each container image, the full registry reference on hover.
- **Older clusters** (k3s v1.23 and other Kubernetes before 1.26) are discovered group by group.
- **Text selection like an editor**: double-click selects a whole IP, name, image or `key=value` label,
  double-click and drag extends by words, triple-click selects the line (a patched egui in `vendor/egui`).
- **Accent color per cluster**: tab title, outline, frame and selections, also on the logs,
  terminals and editors opened from it. New clusters get a free color (a neon one under AMOLED); right-click a
  cluster pill or context for soft and neon swatches or any custom color.
- **Dock tabs**: logs, pod shell, local terminal pinned to a context (no credentials copied), YAML editor.
  A terminal takes the keyboard when it opens, when its tab is picked or when clicked (accent outline),
  wherever the mouse is; a click elsewhere gives it back (patched egui_term in `vendor/egui_term`).
- **Overview**: cluster CPU/memory/pods, per-node usage, live warning events (repeats grouped);
  workloads overview.
- **Helm**: releases, values/manifest/notes, history, rollback, uninstall.

## Develop

```
cargo test
```

`.cargo/config.toml` keeps build output outside OneDrive.

## Installer and releases

Needs WiX (`dotnet tool install --global wix`).

```
build.bat                       # release build; the exe is copied to target\KXS-Watcher\
build.bat msi                   # the same, plus the MSI
.\build_msi.ps1                 # release build + MSI in target\KXS-Watcher\ (Cargo.toml's version)
.\build_msi.ps1 -Version 0.2.0  # same, as MSI version 0.2.0 (tag forms like v.0.2.0 work too)
.\publish_github.ps1 -Tag v0.2.0 -DryRun   # show what would be published
.\publish_github.ps1 -Tag v0.2.0           # push the branch, tag, create the GitHub release, upload the MSI
.\publish_release.ps1                      # same, with the tag v<version from Cargo.toml>
```

The MSI is a wizard: pick the install folder (default `%LocalAppData%\Programs\KXS Watcher`, per user,
no admin prompt), Start menu and Desktop shortcuts, and launch at the end. Any installed version is
replaced (a clean install, downgrades too). Running the same MSI again, or Settings → Installed apps →
KXS Watcher → Modify, opens Change / Repair / Remove; Repair reinstalls every file and shortcut. Its version is the
x.y.z in the tag (Cargo.toml's when the tag has none). Publishing
refuses uncommitted changes and a tag that already exists on another commit.

## Developed by:
- unnamed10110 (trojan.v6@gmail.com)
- Sergio Britos (sergiobritos10110@gmail.com)
