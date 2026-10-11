//! Selecting log text as in an editor: what the keyboard and the pointer do to the selection.

use super::*;

/// The selection of a log tab and the state around it.
#[derive(Default)]
pub(super) struct Select {
    pub(super) sel: Option<Sel>,
    /// Something is selected: a followed log holds still (until End) so it doesn't scroll away.
    pub(super) hold: bool,
    /// The last press was on this log: the keys go here even with the pointer elsewhere.
    pub(super) focused: bool,
    /// Where a press landed inside the selection, and whether the drag out of the window was asked for
    /// already: moving on drags the selected text out, letting go without moving collapses the selection
    /// to that point.
    pub(super) drag_src: Option<(egui::Pos2, bool)>,
    /// Scrolling asked for last frame (a selection dragged past an edge), applied in the next.
    pub(super) scroll_by: egui::Vec2,
}

/// The vertical scroll state a key needs: where the view is, how tall, and the height of a row.
#[derive(Clone, Copy)]
pub(super) struct Scroll {
    pub(super) offset: f32,
    pub(super) height: f32,
    pub(super) row: f32,
}

impl Select {
    /// Shift with the arrows, Page Up/Down and Home/End (Ctrl: of the whole log) selects, as in an
    /// editor; Ctrl+A selects everything, Ctrl+C copies, Escape drops the selection. Returns where to
    /// scroll so the moving end stays on screen.
    pub(super) fn keys(&mut self, ctx: &egui::Context, g: &Geo, marks: &mut [usize; 2], scroll: Scroll) -> Option<f32> {
        use egui::{Key, Modifiers as M};
        #[derive(Clone, Copy)]
        enum Mv {
            Rows(isize),
            Char(isize),
            Line(bool),
            Log(bool),
        }
        let n = g.n;
        let has_sel = self.sel.is_some_and(|s| s.range(*marks).0 != s.range(*marks).1);
        let page = ((scroll.height / scroll.row) as isize - 1).max(1);
        let table = [
            (M::COMMAND | M::SHIFT, Key::Home, Mv::Log(false)),
            (M::COMMAND | M::SHIFT, Key::End, Mv::Log(true)),
            (M::SHIFT, Key::ArrowUp, Mv::Rows(-1)),
            (M::SHIFT, Key::ArrowDown, Mv::Rows(1)),
            (M::SHIFT, Key::PageUp, Mv::Rows(-page)),
            (M::SHIFT, Key::PageDown, Mv::Rows(page)),
            (M::SHIFT, Key::ArrowLeft, Mv::Char(-1)),
            (M::SHIFT, Key::ArrowRight, Mv::Char(1)),
            (M::SHIFT, Key::Home, Mv::Line(false)),
            (M::SHIFT, Key::End, Mv::Line(true)),
        ];
        let (mvs, all, copy, esc) = ctx.input_mut(|i| {
            let mvs: Vec<Mv> = table.iter().flat_map(|&(m, k, mv)| std::iter::repeat_n(mv, i.count_and_consume_key(m, k))).collect();
            let copy = has_sel && i.events.iter().any(|e| matches!(e, egui::Event::Copy));
            if copy {
                i.events.retain(|e| !matches!(e, egui::Event::Copy));
            }
            (mvs, i.consume_key(M::COMMAND, Key::A), copy, has_sel && i.consume_key(M::NONE, Key::Escape))
        });
        if let (true, Some(s)) = (copy, self.sel) {
            ctx.copy_text(g.selected(s, *marks));
        }
        if esc {
            (self.sel, self.hold) = (None, false);
        }
        if all {
            let li = g.line_at(n - 1);
            (self.sel, *marks, self.hold) = (Some(Sel { anchor: (0, 0), head: g.len(li), unit: 1 }), [g.line_at(0), li], true);
        }
        if mvs.is_empty() {
            return None;
        }
        // Without a selection it starts at the first line on screen.
        let mut h = match self.sel {
            Some(s) => (marks[1], s.head),
            None => g.at_row(((scroll.offset / scroll.row).ceil() as usize).min(g.total - 1), 0),
        };
        if self.sel.is_none() {
            (self.sel, marks[0]) = (Some(Sel { anchor: (h.1, h.1), head: h.1, unit: 1 }), h.0);
        }
        for mv in mvs {
            h = match mv {
                Mv::Rows(d) => {
                    let r = g.row_of(h);
                    let col = h.1.saturating_sub((r - g.rows_of(g.vi_of(h.0)).start) * if g.wrap { g.cols } else { 0 });
                    g.at_row(r.saturating_add_signed(d).min(g.total - 1), col)
                }
                Mv::Char(-1) if h.1 == 0 => match g.vi_of(h.0) {
                    0 => h,
                    vi => (g.line_at(vi - 1), g.len(g.line_at(vi - 1))),
                },
                Mv::Char(1) if h.1 >= g.len(h.0) => match g.vi_of(h.0) + 1 {
                    vi if vi < n => (g.line_at(vi), 0),
                    _ => h,
                },
                Mv::Char(d) => (h.0, h.1.saturating_add_signed(d).min(g.len(h.0))),
                Mv::Line(end) => (h.0, if end { g.len(h.0) } else { 0 }),
                Mv::Log(false) => (g.line_at(0), 0),
                Mv::Log(true) => (g.line_at(n - 1), g.len(g.line_at(n - 1))),
            };
        }
        if let Some(s) = &mut self.sel {
            (s.head, s.unit, marks[1]) = (h.1, 1, h.0);
        }
        self.hold = true;
        // Keep the moving end on screen.
        let y = g.row_of(h) as f32 * scroll.row;
        if y < scroll.offset {
            Some(y)
        } else if y + scroll.row > scroll.offset + scroll.height {
            Some(y + scroll.row - scroll.height)
        } else {
            None
        }
    }

    /// The pointer selects: a press starts it (a double press takes the word, a triple the line; with
    /// Shift it extends), dragging moves the end, and past an edge the log scrolls along. A press inside
    /// the selection keeps it instead (it may be dragged out of the window).
    /// `drawn`: the rows on screen, `sel_rects` where the selection is painted, `inner` the log's area.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn pointer(&mut self, ctx: &egui::Context, resp: &egui::Response, g: &Geo, drawn: &[Drawn], sel_rects: &[egui::Rect], marks: &mut [usize; 2], inner: egui::Rect, row_h: f32) {
        let (pressed, down, shift, presses, pos, dt) = ctx.input(|i| (i.pointer.primary_pressed(), i.pointer.primary_down(), i.modifiers.shift, i.pointer.press_count(), i.pointer.interact_pos(), i.stable_dt.min(0.1)));
        if pressed {
            self.focused = resp.is_pointer_button_down_on();
        }
        // The line and char at a point; rows past the ends count as the first or last one on screen.
        let at_pos = |p: egui::Pos2| {
            let (first, last) = (drawn.first()?, drawn.last()?);
            let y = p.y.clamp(first.rect.top(), last.rect.bottom() - 1.0);
            let d = drawn.iter().find(|d| y < d.rect.bottom()).unwrap_or(last);
            Some((d.li, (d.c0 + d.galley.cursor_from_pos(egui::vec2(p.x - d.rect.left(), row_h / 2.0)).index.0).min(d.c1)))
        };
        // A single press inside the selection keeps it: moving on drags the text out of the window to
        // another program, letting go without moving collapses the selection to that point.
        if let (true, Some(p)) = (pressed && presses == 1 && !shift && resp.is_pointer_button_down_on(), pos) {
            self.drag_src = sel_rects.iter().any(|r| r.contains(p)).then_some((p, false));
        }
        let armed = self.drag_src;
        if let Some((p0, sent)) = armed {
            if !down {
                self.drag_src = None;
                if let Some(at) = at_pos(p0).filter(|_| !sent) {
                    (self.sel, *marks) = (Some(Sel { anchor: (at.1, at.1), head: at.1, unit: 1 }), [at.0; 2]);
                    self.hold = false;
                }
            } else if !sent && ctx.input(|i| i.pointer.is_decidedly_dragging()) {
                self.drag_src = Some((p0, true));
                ctx.kxs_request_drag_out(self.sel.map(|s| g.selected(s, *marks)).unwrap_or_default());
            }
        }
        if let (true, Some(p), Some(at)) = (down && resp.is_pointer_button_down_on() && armed.is_none(), pos, pos.and_then(at_pos)) {
            if pressed && !(shift && self.sel.is_some()) {
                let anchor = match presses {
                    1 => (at.1, at.1),
                    2 => word_at(&g.text(at.0), at.1),
                    _ => (0, g.len(at.0)),
                };
                (self.sel, *marks) = (Some(Sel { anchor, head: anchor.1, unit: presses.min(3) }), [at.0; 2]);
            } else if let Some(s) = &mut self.sel {
                let forward = at >= (marks[0], s.anchor.0);
                s.head = match s.unit {
                    1 => at.1,
                    2 => {
                        let w = word_at(&g.text(at.0), at.1);
                        if forward { w.1 } else { w.0 }
                    }
                    _ if forward => g.len(at.0),
                    _ => 0,
                };
                marks[1] = at.0;
            }
            self.hold = self.sel.is_some_and(|s| s.range(*marks).0 != s.range(*marks).1);
            // Past an edge: scroll that way, the faster the farther out.
            let past = |v: f32, lo: f32, hi: f32| if v < lo { v - lo } else if v > hi { v - hi } else { 0.0 };
            let by = egui::vec2(if g.wrap { 0.0 } else { past(p.x, inner.left(), inner.right()) }, past(p.y, inner.top(), inner.bottom()));
            if by != egui::Vec2::ZERO {
                self.scroll_by = -by * 15.0 * dt;
                ctx.request_repaint();
            }
        }
    }
}
