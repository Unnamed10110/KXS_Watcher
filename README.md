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

- **Catalog**: contexts from `~/.kube/config`, `$KUBECONFIG` and any extra files/folders
  (Settings; `~/.kube` is scanned by default). Pick files or folders on disk with the native file
  dialog ("Add kubeconfig…" in the catalog, File menu, Settings). Pin, filter, open several clusters as tabs.
- **Live lists** for every resource kind, including CRDs, built from the API server's Table
  output (the same columns as `kubectl get`, `Wide` = `-o wide`). Multi-namespace filter, search, sort,
  status colors, CPU/memory columns for pods and nodes (metrics-server).
- **Multi-select**: checkboxes, Ctrl/Shift+click, Ctrl+A. Bulk open, merged logs, restart, scale,
  edit YAML, copy names, delete (one confirmation, one summary).
- **Object sub-tabs** inside each cluster tab (double-click a row). Details per kind link to
  everything they reference: owner, node, service account, config maps, secrets, volume claims,
  ingress backends, role bindings, HPA targets, event objects… plus related pods/ReplicaSets/Jobs.
- **Secret / ConfigMap editor**: edit, add and remove keys, Save (conflicts are detected).
- **Actions**: edit YAML, create from YAML (multi-document, server-side apply), delete, scale,
  rollout restart, cordon/uncordon, drain, CronJob trigger/suspend, port-forward.
- **Logs**: one pod or many (interleaved by time, pod name per line), ANSI and log-level colors,
  wrap, timestamps, previous container, Ctrl+F find (plain or regex, only-matching), scrolling to the
  top loads earlier lines, dropped connections resume on their own.
- **Ctrl+K** finds text in the current view (lists, details, YAML, logs). Collapsed sections with
  matches open; hidden secret values are searched too and flagged without being revealed.
- **Search configs & secrets**: text (plain or regex) in every Config Map and Secret value, key and
  name, in all or the selected namespaces. Secret values stay masked unless shown; a click opens the
  object with the same text highlighted.
- **Themes**: Dark, Light, System, AMOLED (pure black) in cyan, red and green, Crimson, Forest,
  Ocean and Violet (Settings or View → Theme).
- **Accent color per cluster**: tab title, outline, frame and selections, also on the logs,
  terminals and editors opened from it. New clusters get a free color; right-click a context to pick
  another swatch or any custom color.
- **Dock tabs**: logs, pod shell, local terminal pinned to a context (no credentials copied), YAML editor.
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
.\build_msi.ps1                 # release build + MSI in target\KXS-Watcher\
.\publish_github.ps1 -Tag v0.2.0 -DryRun   # show what would be published
.\publish_github.ps1 -Tag v0.2.0           # push the branch, tag, create the GitHub release, upload the MSI
.\publish_release.ps1                      # same, with the tag v<version from Cargo.toml>
```

The MSI is a wizard: pick the install folder (default `%LocalAppData%\Programs\KXS Watcher`, per user,
no admin prompt), Start menu and Desktop shortcuts, and launch at the end. Any installed version is
replaced (a clean install, downgrades too); running the same MSI again reinstalls it. Its version is the
x.y.z in the tag (Cargo.toml's when the tag has none). Publishing
refuses uncommitted changes and a tag that already exists on another commit.
