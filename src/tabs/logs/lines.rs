//! Drawing the lines of a log tab: the find scan, the rows (with Wrap), scrolling to things, and the paint.

use super::*;

pub(super) const TS_WIDTH: usize = 24; // "2026-09-26 08:10:46.495 "

pub(super) const SRC_COLORS: [Color32; 8] = [
    Color32::from_rgb(86, 156, 214), Color32::from_rgb(197, 134, 192), Color32::from_rgb(78, 201, 176), Color32::from_rgb(220, 160, 90),
    Color32::from_rgb(156, 220, 254), Color32::from_rgb(214, 157, 133), Color32::from_rgb(181, 206, 168), Color32::from_rgb(255, 128, 128),
];

/// A log row on screen this frame: its line, chars `c0..c1` of it, where, and how it was laid out.
pub(super) struct Drawn {
    pub(super) li: usize,
    pub(super) c0: usize,
    pub(super) c1: usize,
    pub(super) rect: egui::Rect,
    pub(super) galley: Arc<egui::Galley>,
}

/// Byte offset of the `n`th char (or the end).
pub(super) fn char_byte(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map_or(s.len(), |(i, _)| i)
}

pub(super) fn slice_job(job: &LayoutJob, a: usize, b: usize) -> LayoutJob {
    let mut out = LayoutJob { text: job.text[a..b].to_string(), ..Default::default() };
    for s in &job.sections {
        let (sa, sb) = (s.byte_range.start.0.max(a), s.byte_range.end.0.min(b));
        if sa < sb {
            out.sections.push(LayoutSection { leading_space: 0.0, byte_range: ByteIndex(sa - a)..ByteIndex(sb - a), format: s.format.clone() });
        }
    }
    out
}

impl LogTab {
    /// How lines are shown (time, pod name), borrowed from the tab's options.
    pub(super) fn display(&self) -> Display<'_> {
        Display { show_ts: self.show_ts, show_src: self.show_src, tz: &self.tz, sources: &self.sources }
    }

    /// A line as shown (timestamp and pod name as they are on).
    pub(super) fn shown(&self, l: &Line) -> String {
        self.display().text(l)
    }

    pub(super) fn jump_to_start(&mut self) {
        (self.follow, self.set_offset) = (false, Some(0.0));
    }

    pub(super) fn jump_to_end(&mut self) {
        (self.follow, self.set_offset, self.away_from, self.select.hold) = (true, Some(f32::MAX), None, false);
    }

    pub(super) fn visible_text(&self) -> String {
        let b = self.buf.locked();
        let idx: Box<dyn Iterator<Item = usize>> = match &self.view {
            Some(v) => Box::new(v.iter().map(|&i| i as usize)),
            None => Box::new(0..b.lines.len()),
        };
        idx.filter_map(|i| b.lines.get(i)).map(|l| self.shown(l)).collect::<Vec<_>>().join("\n")
    }

    /// The selected text, lines as shown (only the matching ones with Only matching).
    #[cfg(test)]
    pub(super) fn selected_text(&self, b: &LogBuf) -> String {
        self.select.sel.map(|s| selected_text(&self.display(), self.view.as_deref(), &b.lines, s, b.marks)).unwrap_or_default()
    }

    /// Match scan: on changes, throttled while lines stream in (regex over 100k lines isn't free).
    fn scan_matches(&mut self, ui: &egui::Ui, b: &LogBuf, matcher: &Option<Matcher>) {
        let key = (self.epoch, b.rev, self.find.query.clone(), self.find.regex, self.find.case, self.only_matching && matcher.is_some());
        let opts_changed = (&key.2, key.3, key.4, key.5) != (&self.scan_key.2, self.scan_key.3, self.scan_key.4, self.scan_key.5) || key.0 != self.scan_key.0;
        // Without a search there is nothing to scan: no reason to wait, or to ask for a frame later.
        if key != self.scan_key && (opts_changed || matcher.is_none() || self.scanned.elapsed() > Duration::from_millis(250)) {
            let mut total = 0u32;
            self.hits.clear();
            if let Some(m) = matcher {
                for (i, l) in b.lines.iter().enumerate() {
                    let n = m.count(&l.text) as u32;
                    if n > 0 {
                        self.hits.push((i as u32, total));
                        total += n;
                    }
                }
            }
            self.view = key.5.then(|| self.hits.iter().map(|h| h.0).collect());
            self.find.set_total(total as usize);
            (self.scan_key, self.scanned) = (key, Instant::now());
        } else if key != self.scan_key {
            ui.ctx().request_repaint_after(Duration::from_millis(260));
        }
    }

    /// Wrap: long lines become several fixed-width rows (`cols` chars), so every row has the same height.
    fn wrap_rows(&mut self, b: &LogBuf, n: usize, cols: usize) {
        let rkey = (self.epoch, b.rev, n, self.wrap, self.show_ts, if self.wrap { cols + usize::from(self.show_src) * 100_000 } else { 0 });
        if self.wrap && self.rows_key != rkey {
            self.rows.clear();
            let mut acc = 0u32;
            let disp = Display { show_ts: self.show_ts, show_src: self.show_src, tz: &self.tz, sources: &self.sources };
            for vi in 0..n {
                self.rows.push(acc);
                let l = &b.lines[self.view.as_ref().map_or(vi, |v| v[vi] as usize)];
                acc += ((disp.prefix_chars(l) + l.chars as usize).div_ceil(cols)).max(1) as u32;
            }
            self.rows.push(acc);
            self.rows_key = rkey;
        }
    }

    pub(super) fn lines_ui(&mut self, ui: &mut egui::Ui) {
        crate::prof!("LogTab::lines_ui");
        let buf = self.buf.clone();
        let mut guard = buf.locked();
        let b = &mut *guard;
        let matcher = self.find.matcher();
        self.scan_matches(ui, b, &matcher);

        let n = self.view.as_ref().map_or(b.lines.len(), Vec::len);
        let font = TextStyle::Monospace.resolve(ui.style());
        let (row_h, gw) = ui.fonts_mut(|f| (f.row_height(&font), f.glyph_width(&font, '0')));
        let row_sp = row_h + ui.spacing().item_spacing.y;
        let cols = ((ui.available_width() - 16.0) / gw).max(20.0) as usize;
        self.wrap_rows(b, n, cols);
        let total = if self.wrap { self.rows.last().copied().unwrap_or(0) as usize } else { n };
        let g = Geo { lines: &b.lines, view: self.view.as_deref(), rows: &self.rows, wrap: self.wrap, cols, n, total, disp: Display { show_ts: self.show_ts, show_src: self.show_src, tz: &self.tz, sources: &self.sources } };

        // Keep the first line in place after loading earlier lines.
        if let Some((ts, src, text)) = self.anchor.clone().filter(|_| self.earlier.is_none()) {
            if let Some(vi) = (0..n).find(|&vi| b.lines.get(g.line_at(vi)).is_some_and(|l| l.ts == ts && l.src == src && l.text == text)) {
                self.set_offset = Some(g.first_row(vi) as f32 * row_sp);
            }
            self.anchor = None;
        }
        // Jump to the current match.
        if self.find.scroll && self.find.total > 0 {
            let k = self.find.current as u32;
            if let Some(&(line, _)) = self.hits.get(self.hits.partition_point(|h| h.1 <= k).wrapping_sub(1)) {
                let vi = self.view.as_ref().map_or(Some(line as usize), |v| v.iter().position(|&x| x == line));
                if let Some(vi) = vi {
                    self.follow = false;
                    self.set_offset = Some((g.first_row(vi) as f32 * row_sp - self.view_h / 2.0).max(0.0));
                }
            }
            self.find.scroll = false;
        }

        let (text_color, weak) = (ui.visuals().text_color(), ui.visuals().weak_text_color());
        let area = ui.available_rect_before_wrap();
        // Keys go to the log under the pointer, else to the one clicked last; not from a text field or
        // a terminal on screen that has them.
        let term = TERM_KEYS.with(|t| t.get()).is_some_and(|p| p + 1 >= ui.ctx().cumulative_pass_nr());
        let keys = (ui.rect_contains_pointer(area) || self.select.focused) && !ui.ctx().text_edit_focused() && !term;
        let scroll = Scroll { offset: self.last_off, height: self.view_h, row: row_sp };
        if keys && n > 0 && let Some(y) = self.select.keys(ui.ctx(), &g, &mut b.marks, scroll) {
            self.set_offset = Some(y);
        }

        // Under the scroll bars (added later, so they stay on top): presses and drags that select.
        let resp = ui.interact(area, ui.id().with("log-select"), egui::Sense::click_and_drag());
        let mut sa = egui::ScrollArea::new([!self.wrap, true]).id_salt(("logs", &self.title, self.epoch)).auto_shrink(false).stick_to_bottom(self.follow && !self.select.hold);
        if let Some(y) = self.set_offset.take() {
            // Past the end show_rows draws no rows and the offset turns infinite: a black log for good.
            sa = sa.vertical_scroll_offset(y.min((total as f32 * row_sp - self.view_h).max(0.0)));
        }
        let max_w = (b.max_chars as usize + 60) as f32 * gw;
        let scroll_by = std::mem::take(&mut self.select.scroll_by);
        let sel_range = self.select.sel.map(|s| s.range(b.marks)).filter(|(a, e)| a != e);
        let sel_color = ui.visuals().selection.bg_fill;
        let mut drawn: Vec<Drawn> = vec![];
        let mut sel_rects: Vec<egui::Rect> = vec![];
        let out = sa.show_rows(ui, row_h, total, |ui, range| {
            // The arrows and Page Up/Down scroll.
            let mut by = scroll_by;
            if keys {
                let page = (scroll.height - row_sp).max(row_sp);
                by += ui.input_mut(|i| {
                    let mut k = |key, by: egui::Vec2| by * i.count_and_consume_key(egui::Modifiers::NONE, key) as f32;
                    k(egui::Key::ArrowUp, egui::vec2(0.0, row_sp)) + k(egui::Key::ArrowDown, egui::vec2(0.0, -row_sp))
                        + k(egui::Key::PageUp, egui::vec2(0.0, page)) + k(egui::Key::PageDown, egui::vec2(0.0, -page))
                        + k(egui::Key::ArrowLeft, egui::vec2(4.0 * gw, 0.0)) + k(egui::Key::ArrowRight, egui::vec2(-4.0 * gw, 0.0))
                });
            }
            if by != egui::Vec2::ZERO {
                ui.scroll_with_delta_animation(by, egui::style::ScrollAnimation::none());
            }
            if !g.wrap {
                ui.set_min_width(max_w);
            }
            let (left, top, first) = (ui.max_rect().left(), ui.max_rect().top(), range.start);
            for r in range {
                let (vi, part) = if g.wrap { (g.rows.partition_point(|&p| p as usize <= r).saturating_sub(1), 0) } else { (r, 0) };
                let part = if g.wrap { r - g.first_row(vi) } else { part };
                let li = g.line_at(vi);
                let Some(l) = g.lines.get(li) else { continue };
                let (hits, cur) = match &matcher {
                    Some(m) => {
                        let hits = m.ranges(&l.text);
                        // Global index of this line's first match (prefix sums from the scan).
                        let before = self.hits.get(self.hits.partition_point(|h| (h.0 as usize) < li)).map_or(self.find.total, |h| h.1 as usize);
                        let cur = self.find.current.checked_sub(before).filter(|&c| c < hits.len());
                        (hits, cur)
                    }
                    None => (vec![], None),
                };
                let job = g.disp.job(l, &font, text_color, weak, &hits, cur);
                let job = if g.wrap { slice_job(&job, char_byte(&job.text, part * cols), char_byte(&job.text, (part + 1) * cols)) } else { job };
                let chars = g.len(li);
                let (c0, c1) = if g.wrap { (part * cols, ((part + 1) * cols).min(chars)) } else { (0, chars) };
                let galley = ui.fonts_mut(|f| f.layout_job(job));
                let rect = egui::Rect::from_min_size(egui::pos2(left, top + (r - first) as f32 * row_sp), egui::vec2(galley.size().x, row_h));
                // The selection behind the text; a line selected up to its end shows its line break.
                if let Some(((l0, s0), (l1, e1))) = sel_range
                    && (l0..=l1).contains(&li)
                {
                    let s = if li == l0 { s0 } else { 0 }.max(c0);
                    let (e, eol) = if li == l1 { (e1.min(c1), false) } else { (c1, c1 >= chars) };
                    if s < e || (eol && s <= e) {
                        let x = |c: usize| rect.left() + galley.pos_from_cursor(egui::text::CCursor::new(c - c0)).min.x;
                        let x1 = if eol { x(e) + gw * 0.6 } else { x(e) };
                        let hl = egui::Rect::from_x_y_ranges(x(s)..=x1, rect.y_range());
                        ui.painter().rect_filled(hl, 0.0, sel_color);
                        sel_rects.push(hl);
                    }
                }
                ui.painter().galley(rect.min, galley.clone(), text_color);
                drawn.push(Drawn { li, c0, c1, rect, galley });
            }
        });

        if resp.hovered() {
            ui.set_cursor_icon(egui::CursorIcon::Text);
        }
        self.select.pointer(ui.ctx(), &resp, &g, &drawn, &sel_rects, &mut b.marks, out.inner_rect, row_h);
        let (lines, appended) = (b.lines.len(), b.appended);
        // Matches on the scroll track, the current one white (at most one per pixel row).
        if !self.hits.is_empty() && lines > 0 {
            let track = out.inner_rect;
            let mut last_y = f32::NEG_INFINITY;
            for (k, &(li, first)) in self.hits.iter().enumerate() {
                let y = track.top() + track.height() * li as f32 / lines as f32;
                let next = self.hits.get(k + 1).map_or(self.find.total as u32, |h| h.1);
                let current = (first as usize..next as usize).contains(&self.find.current);
                if y - last_y >= 1.0 || current {
                    let color = if current { Color32::WHITE } else { find::HIT };
                    ui.painter().rect_filled(egui::Rect::from_min_size(egui::pos2(track.right() - 5.0, y - 1.0), egui::vec2(5.0, 3.0)), 1.0, color);
                    last_y = y;
                }
            }
        }
        drop(guard);
        let off = out.state.offset.y;
        if off <= 0.0 && self.last_off > 0.0 {
            self.load_earlier(ui.ctx()); // reached the top: fetch older lines
        }
        (self.last_off, self.view_h) = (off, out.inner_rect.height());
        // Away from the bottom with new lines coming in: offer to jump there.
        let at_bottom = off + out.inner_rect.height() >= out.content_size.y - row_h;
        if at_bottom {
            self.away_from = None;
        } else {
            let from = *self.away_from.get_or_insert(appended);
            let new = appended.saturating_sub(from);
            if new > 0 {
                let text = format!("⬇ {new} new line{} · Go to end", if new == 1 { "" } else { "s" });
                let size = egui::vec2(ui.fonts_mut(|f| f.layout_no_wrap(text.clone(), FontId::proportional(13.0), Color32::WHITE).size().x) + 36.0, 34.0);
                let at = egui::Rect::from_min_size(out.inner_rect.right_bottom() - size - egui::vec2(24.0, 14.0), size);
                let pill = egui::Button::new(RichText::new(text).strong()).corner_radius(17.0).stroke(egui::Stroke::new(1.0, ui.visuals().selection.stroke.color));
                if ui.put(at, pill).clicked() {
                    self.jump_to_end();
                }
            }
        }
        if keys {
            let (home, end) = ui.input_mut(|i| (i.consume_key(egui::Modifiers::COMMAND, egui::Key::Home), i.consume_key(egui::Modifiers::COMMAND, egui::Key::End)));
            if home {
                self.jump_to_start();
            }
            if end {
                self.jump_to_end();
            }
        }
    }
}
