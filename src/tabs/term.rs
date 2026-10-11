//! Terminal tabs: a local shell, pod exec, node shell and drain, and the command lines for them.

use super::*;

/// egui_term panics if its event receiver goes away; the App keeps the receiver for its lifetime.
pub static TERM_TX: OnceLock<Sender<(u64, PtyEvent)>> = OnceLock::new();

pub struct TermTab {
    pub title: String,
    /// A local terminal for this context (id): reopened on the next start.
    pub local: Option<String>,
    backend: TerminalBackend,
    /// Pod this terminal owns (node shell): deleted when the terminal goes away.
    pub cleanup: Option<Cleanup>,
    /// Pass it was last drawn in: coming back on screen (opened, its tab picked) takes the keyboard.
    drawn: u64,
}

thread_local! {
    /// The terminal that takes the keyboard (backend id): the one opened, clicked or brought up
    /// last, until a click somewhere else. Only one at a time, even with several on screen.
    static KEYBOARD: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    /// The pass a terminal holding the keyboard was last drawn in: the logs leave the keys to it.
    pub(super) static TERM_KEYS: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Lines of scrollback of the next terminals to open (Settings): a line costs memory for as long as it
/// is kept, and the default of alacritty (10,000) is more than most sessions need.
static SCROLLBACK: AtomicUsize = AtomicUsize::new(5_000);

pub fn set_scrollback(lines: usize) {
    SCROLLBACK.store(lines, Ordering::Relaxed);
}

pub type Command = (String, Vec<String>);

/// (client, namespace, pod) to delete.
pub type Cleanup = (Client, String, String);

impl TermTab {
    pub fn new(ctx: &egui::Context, id: u64, title: String, (shell, args): Command) -> std::io::Result<Self> {
        let tx = TERM_TX.get().cloned().ok_or_else(|| std::io::Error::other("the terminal event channel is not set up"))?;
        let settings = BackendSettings { shell, args, working_directory: kubeconfig::home(), scrollback: SCROLLBACK.load(Ordering::Relaxed) };
        Ok(TermTab { title, local: None, backend: TerminalBackend::new(id, ctx.clone(), tx, settings)?, cleanup: None, drawn: 0 })
    }

    /// Whether typing goes here (the app leaves its shortcuts to the terminal then).
    pub fn has_keyboard(&self) -> bool {
        KEYBOARD.with(|k| k.get()) == Some(self.backend.id())
    }

    /// `size`: the terminal's font size factor (Settings), on egui_term's 14 px.
    pub fn ui(&mut self, ui: &mut egui::Ui, size: f32) {
        let id = self.backend.id();
        let area = ui.max_rect();
        // Where a button went down this frame (a tap can press and release in one frame, so not
        // `press_origin`, which is gone by then).
        let press = ui.input(|i| i.events.iter().rev().find_map(|e| match e {
            egui::Event::PointerButton { pressed: true, pos, .. } => Some(*pos),
            _ => None,
        }));
        if let Some(p) = press {
            if area.contains(p) {
                KEYBOARD.with(|k| k.set(Some(id)));
            } else if self.has_keyboard() {
                KEYBOARD.with(|k| k.set(None)); // clicked elsewhere
            }
        }
        let pass = ui.ctx().cumulative_pass_nr();
        if self.drawn + 1 < pass {
            KEYBOARD.with(|k| k.set(Some(id))); // just opened, or its tab was picked
        }
        self.drawn = pass;
        let keys = self.has_keyboard();
        if keys {
            TERM_KEYS.with(|t| t.set(Some(pass)));
        }
        let font = TerminalFont::new(FontSettings { font_type: FontId::monospace(14.0 * size) });
        let view = TerminalView::new(ui, &mut self.backend).set_focus(keys).set_size(ui.available_size()).set_font(font);
        let r = ui.add(view);
        if keys {
            let accent = crate::ui_kit::tokens(ui).accent;
            ui.painter().rect_stroke(r.rect, 0.0, egui::Stroke::new(1.0, accent), egui::StrokeKind::Inside);
        }
    }
}

impl Drop for TermTab {
    fn drop(&mut self) {
        if self.has_keyboard() {
            KEYBOARD.with(|k| k.set(None));
        }
        // ponytail: if the app is killed mid-session the pod stays until its 4h sleep ends.
        if let Some((client, ns, pod)) = self.cleanup.take() {
            tokio::spawn(ops::delete(client, ApiResource::erase::<Pod>(&()), ns, pod));
        }
    }
}

pub fn kubectl_line(k: &Ctx, rest: &str) -> String {
    format!("kubectl --kubeconfig \"{}\" --context \"{}\" {rest}", k.file.display(), k.name)
}

/// alacritty passes args raw on Windows, so hand cmd.exe one pre-quoted line.
fn cmd_line(line: String) -> Command {
    ("cmd.exe".into(), vec!["/s".into(), "/c".into(), format!("\"{line}\"")])
}

pub fn local_shell(k: &Ctx) -> Command {
    let kc = kubeconfig::terminal_kubeconfig(k).unwrap_or_default().replace('\'', "''");
    let name = k.name.replace('\'', "''");
    let script = format!("$env:KUBECONFIG='{kc}'; Write-Host 'kubectl context: {name}' -ForegroundColor Cyan");
    ("powershell.exe".into(), vec!["-NoLogo".into(), "-NoExit".into(), "-Command".into(), script])
}

pub fn pod_shell(k: &Ctx, ns: &str, pod: &str, container: Option<&str>) -> Command {
    let c = container.map(|c| format!(" -c {c}")).unwrap_or_default();
    cmd_line(kubectl_line(k, &format!("-n {ns} exec -it {pod}{c} -- sh -c \"clear; (bash || ash || sh)\" || pause")))
}

/// Shell in the node's own namespaces, through a node-shell pod.
pub fn node_exec(k: &Ctx, ns: &str, pod: &str) -> Command {
    cmd_line(kubectl_line(k, &format!("-n {ns} exec -it {pod} -- nsenter -t 1 -m -u -i -n -p -- sh -c \"clear; (bash || ash || sh)\" || pause")))
}

pub fn drain(k: &Ctx, node: &str) -> Command {
    cmd_line(kubectl_line(k, &format!("drain {node} --ignore-daemonsets --delete-emptydir-data & pause")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Harness;

    /// A real PTY running `cmd.exe`: `lines` lines of 110 chars, then it waits. Returns once the last
    /// line is on screen.
    async fn busy_terminal(h: &mut Harness, lines: usize) -> TermTab {
        let (tx, rx) = std::sync::mpsc::channel();
        std::mem::forget(rx); // egui_term panics when nobody receives: it must outlive every terminal
        let _ = TERM_TX.set(tx);
        let text = "abcdefghijklmnopqrstuvwxyz0123456789 ".repeat(3);
        let mut t = TermTab::new(&h.ctx, 1, "bench".into(), cmd_line(format!("(for /l %i in (1,1,{lines}) do @echo %i {text}) & pause"))).expect("a PTY");
        for _ in 0..400 {
            h.frame(vec![], |ui| t.ui(ui, 1.0));
            if t.backend.sync().cells.iter().map(|i| i.c).collect::<String>().contains(&format!("{lines} abcdef")) {
                return t;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let seen: String = t.backend.sync().cells.iter().map(|i| i.c).collect();
        panic!("the terminal never printed its last line: {:?}", seen.trim());
    }

    /// The screen is copied for drawing only when the terminal changed: output, or something done to it.
    #[tokio::test]
    async fn the_screen_is_copied_only_when_something_changed() {
        let mut h = Harness::themed(egui::vec2(900.0, 500.0));
        let mut t = busy_terminal(&mut h, 300).await;
        let copies = |t: &mut TermTab| t.backend.sync().generation;
        let before = copies(&mut t);
        for _ in 0..5 {
            h.frame(vec![], |ui| t.ui(ui, 1.0));
        }
        assert_eq!(copies(&mut t), before, "nothing changed: nothing copied");
        t.backend.process_command(egui_term::BackendCommand::Scroll(3));
        assert_eq!(copies(&mut t), before + 1, "scrolling is a change");
        assert_eq!(copies(&mut t), before + 1);
        let screen: String = t.backend.sync().cells.iter().map(|i| i.c).collect();
        assert!(!screen.contains("300 abcdef"), "scrolled up: the last line is below the screen");
    }

    /// Text is painted a row at a time, not a cell at a time, with every glyph still on its cell.
    #[tokio::test]
    async fn text_is_drawn_in_runs_on_the_cell_grid() {
        let mut h = Harness::themed(egui::vec2(900.0, 500.0));
        let mut t = busy_terminal(&mut h, 60).await;
        let out = h.frame_out(vec![], |ui| t.ui(ui, 1.0));
        let texts: Vec<_> = out.shapes.iter().filter_map(|c| if let egui::Shape::Text(t) = &c.shape { Some(t) } else { None }).collect();
        let cell = egui_term::TerminalFont::new(egui_term::FontSettings { font_type: FontId::monospace(14.0) }).font_measure(&h.ctx).width.floor();
        let long: Vec<_> = texts.iter().filter(|t| t.galley.job.text.contains("abcdefghijklmnopqrstuvwxyz0123456789 abc")).collect();
        assert!(!long.is_empty() && texts.len() < 80, "a few shapes per row, not one per cell: {} shapes", texts.len());
        for t in long {
            let xs: Vec<f32> = t.galley.rows[0].glyphs.iter().map(|g| g.pos.x).collect();
            assert!(xs.len() > 100, "the whole line is one text");
            for (i, x) in xs.iter().enumerate() {
                assert!((x - xs[0] - cell * i as f32).abs() < 0.01, "glyph {i} at {x}, cells {cell} wide");
            }
        }
    }

    /// What a terminal on screen costs while nothing happens in it (the frame the app pays whenever
    /// something else asks for one), with a full scrollback behind it.
    #[tokio::test]
    #[ignore]
    async fn bench_terminal_frame_full_scrollback() {
        let mut h = Harness::themed(egui::vec2(1200.0, 800.0));
        let mut t = busy_terminal(&mut h, 12_000).await;
        h.frame(vec![], |ui| t.ui(ui, 1.0));
        crate::prof::bench("terminal frame, 10k lines of history, idle", 30, || h.frame(vec![], |ui| t.ui(ui, 1.0)));
        crate::prof::bench("terminal sync() alone", 30, || {
            t.backend.sync();
        });
    }
}
