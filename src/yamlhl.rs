//! YAML syntax colors: a small tokenizer for the YAML the cluster hands out and the user edits. It
//! replaces the syntect grammar, which took ~9 ms per KB (a keystroke in a 5 KB object felt it) and
//! 130 ms to load its definitions the first time; this one is a single pass over the text.
//!
//! What it knows: comments, keys, `-` items, quoted / plain scalars (numbers and `true`/`false`/`null`
//! apart), block scalars (`|` and `>`: every deeper line is text), anchors / aliases / tags, flow
//! collections on one line. What it leaves out: multi-line quoted or plain scalars and `? ` keys are
//! colored line by line, like whatever each line looks like.

use egui::text::{ByteIndex, LayoutJob, TextFormat};
use egui::{Color32, FontId};

struct Palette {
    text: Color32,
    key: Color32,
    string: Color32,
    number: Color32,
    keyword: Color32,
    comment: Color32,
    punct: Color32,
    anchor: Color32,
}

fn palette(dark: bool, text: Color32) -> Palette {
    let c = Color32::from_rgb;
    if dark {
        Palette { text, key: c(137, 180, 250), string: c(166, 227, 161), number: c(250, 179, 135), keyword: c(203, 166, 247), comment: c(127, 132, 156), punct: c(147, 153, 178), anchor: c(148, 226, 213) }
    } else {
        Palette { text, key: c(30, 102, 245), string: c(64, 160, 43), number: c(210, 90, 10), keyword: c(136, 57, 239), comment: c(140, 143, 161), punct: c(108, 111, 133), anchor: c(23, 146, 153) }
    }
}

/// The text as one job, colored: `color` for what has none of its own, `dark` picks the palette.
pub fn job(text: &str, font: &FontId, color: Color32, dark: bool) -> LayoutJob {
    let p = palette(dark, color);
    let mut out = Out { job: LayoutJob::default(), font, color: None };
    out.job.text.reserve(text.len());
    // Content of a block scalar: the lines indented deeper than this column belong to it.
    let mut block: Option<usize> = None;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        tokens(&mut out, &p, body, &mut block);
        out.push(&line[body.len()..], p.text);
    }
    out.job
}

struct Out<'a> {
    job: LayoutJob,
    font: &'a FontId,
    /// Color of the last section, to extend it instead of starting another.
    color: Option<Color32>,
}

impl Out<'_> {
    fn push(&mut self, s: &str, color: Color32) {
        if s.is_empty() {
            return;
        }
        self.job.text.push_str(s);
        match self.job.sections.last_mut() {
            Some(last) if self.color == Some(color) => last.byte_range.end = ByteIndex(self.job.text.len()),
            _ => {
                let start = self.job.text.len() - s.len();
                self.job.sections.push(egui::text::LayoutSection { leading_space: 0.0, byte_range: ByteIndex(start)..ByteIndex(self.job.text.len()), format: TextFormat::simple(self.font.clone(), color) });
                self.color = Some(color);
            }
        }
    }
}

/// The leading spaces of `s` and the rest.
fn spaces(s: &str) -> (&str, &str) {
    s.split_at(s.len() - s.trim_start_matches(' ').len())
}

fn tokens(out: &mut Out, p: &Palette, body: &str, block: &mut Option<usize>) {
    let (indent_ws, rest) = spaces(body);
    let indent = indent_ws.len();
    if let Some(parent) = *block {
        if rest.is_empty() || indent > parent {
            out.push(indent_ws, p.text);
            out.push(rest, p.string);
            return;
        }
        *block = None;
    }
    out.push(indent_ws, p.text);
    let (mut s, mut col) = (rest, indent);
    if s.starts_with('#') {
        return out.push(s, p.comment);
    }
    if indent == 0 && (s.starts_with("---") || s.starts_with("...")) && s[3..].chars().next().is_none_or(|c| c == ' ') {
        out.push(&s[..3], p.punct);
        let (ws, after) = spaces(&s[3..]);
        out.push(ws, p.text);
        (s, col) = (after, 3 + ws.len());
    }
    // Sequence items, possibly nested (`- - a`); a block scalar after one belongs to its column.
    let mut parent = col;
    while s == "-" || s.starts_with("- ") {
        parent = col;
        out.push("-", p.punct);
        let (ws, after) = spaces(&s[1..]);
        out.push(ws, p.text);
        (s, col) = (after, col + 1 + ws.len());
    }
    if let Some(k) = key_end(s) {
        out.push(&s[..k], p.key);
        out.push(":", p.punct);
        let (ws, after) = spaces(&s[k + 1..]);
        out.push(ws, p.text);
        (s, parent) = (after, col);
    }
    value(out, p, s, parent, block);
}

/// Where the `:` that ends the key at the start of `s` is, if there is a key.
fn key_end(s: &str) -> Option<usize> {
    let is_colon = |t: &str| t.starts_with(':') && t[1..].chars().next().is_none_or(|c| c == ' ');
    match s.chars().next()? {
        '"' | '\'' => {
            let end = quoted_end(s)?;
            let (ws, after) = spaces(&s[end..]);
            is_colon(after).then_some(end + ws.len())
        }
        '{' | '[' | '#' | '|' | '>' | '&' | '*' | '!' => None,
        _ => {
            let b = s.as_bytes();
            let mut i = 0;
            while i < b.len() {
                match b[i] {
                    b':' if is_colon(&s[i..]) => return Some(i),
                    b'#' if i > 0 && b[i - 1] == b' ' => return None,
                    _ => i += 1,
                }
            }
            None
        }
    }
}

/// The end (after the closing quote, or of the line) of the quoted scalar at the start of `s`.
fn quoted_end(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let q = *b.first()?;
    let mut i = 1;
    while i < b.len() {
        match b[i] {
            b'\\' if q == b'"' => i += 2,
            c if c == q && q == b'\'' && b.get(i + 1) == Some(&b'\'') => i += 2, // '' is a quote inside
            c if c == q => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// What follows a key (or a `-`): properties, then the scalar, block header, flow collection or comment.
fn value(out: &mut Out, p: &Palette, mut s: &str, parent: usize, block: &mut Option<usize>) {
    // Anchors, aliases and tags in front of the value.
    while matches!(s.chars().next(), Some('&' | '*' | '!')) {
        let end = s.find(' ').unwrap_or(s.len());
        out.push(&s[..end], p.anchor);
        let (ws, after) = spaces(&s[end..]);
        out.push(ws, p.text);
        s = after;
    }
    let Some(first) = s.chars().next() else { return };
    match first {
        '#' => out.push(s, p.comment),
        '|' | '>' => {
            let end = s.find([' ', '#']).unwrap_or(s.len());
            out.push(&s[..end], p.keyword);
            let (ws, after) = spaces(&s[end..]);
            out.push(ws, p.text);
            out.push(after, p.comment);
            *block = Some(parent);
        }
        '"' | '\'' => {
            let end = quoted_end(s).unwrap_or(s.len());
            out.push(&s[..end], p.string);
            trailing(out, p, &s[end..]);
        }
        '{' | '[' => flow(out, p, s),
        _ => {
            let b = s.as_bytes();
            let end = (1..b.len()).find(|&i| b[i] == b'#' && b[i - 1] == b' ').map_or(s.len(), |i| i - 1);
            let (token, after) = s.split_at(end);
            let word = token.trim_end_matches(' ');
            out.push(word, scalar_color(p, word));
            trailing(out, p, &token[word.len()..]);
            trailing(out, p, after);
        }
    }
}

/// What is left of a line after a value: spaces, then maybe a comment.
fn trailing(out: &mut Out, p: &Palette, s: &str) {
    let (ws, rest) = spaces(s);
    out.push(ws, p.text);
    out.push(rest, if rest.starts_with('#') { p.comment } else { p.text });
}

fn scalar_color(p: &Palette, t: &str) -> Color32 {
    match t {
        "true" | "false" | "null" | "~" | "True" | "False" | "Null" | "TRUE" | "FALSE" | "NULL" => p.keyword,
        _ if is_number(t) => p.number,
        _ => p.string,
    }
}

fn is_number(t: &str) -> bool {
    let digits = t.trim_start_matches(['+', '-']);
    if let Some(hex) = digits.strip_prefix("0x") {
        return !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit());
    }
    matches!(digits.chars().next(), Some('0'..='9' | '.')) && (digits.starts_with(".inf") || digits.starts_with(".nan") || t.parse::<f64>().is_ok())
}

/// A flow collection (`{a: 1, b: [x, "y"]}`) on one line: punctuation, keys, quoted and plain scalars.
fn flow(out: &mut Out, p: &Palette, s: &str) {
    let b = s.as_bytes();
    let (mut i, mut word) = (0, None::<usize>);
    // The pending plain word, if any, in `color`.
    let flush = |out: &mut Out, word: &mut Option<usize>, end: usize, key: bool| {
        if let Some(start) = word.take() {
            let w = s[start..end].trim_end_matches(' ');
            out.push(w, if key { p.key } else { scalar_color(p, w) });
            out.push(&s[start + w.len()..end], p.text);
        }
    };
    while i < b.len() {
        match b[i] {
            b'{' | b'}' | b'[' | b']' | b',' => {
                flush(out, &mut word, i, false);
                out.push(&s[i..i + 1], p.punct);
                i += 1;
            }
            b'"' | b'\'' if word.is_none() => {
                let end = i + quoted_end(&s[i..]).unwrap_or(s.len() - i);
                out.push(&s[i..end], p.string);
                i = end;
            }
            b':' if b.get(i + 1).is_none_or(|n| matches!(n, b' ' | b',' | b'}' | b']')) => {
                flush(out, &mut word, i, true);
                out.push(":", p.punct);
                i += 1;
            }
            b'#' if i > 0 && b[i - 1] == b' ' => {
                flush(out, &mut word, i, false);
                out.push(&s[i..], p.comment);
                return;
            }
            b' ' if word.is_none() => {
                out.push(" ", p.text);
                i += 1;
            }
            _ => {
                word.get_or_insert(i);
                i += 1;
            }
        }
    }
    flush(out, &mut word, b.len(), false);
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: Color32 = Color32::WHITE;

    fn colored(src: &str) -> Vec<(String, Color32)> {
        let j = job(src, &FontId::monospace(12.0), TEXT, true);
        j.sections.iter().map(|s| (j.text[s.byte_range.start.0..s.byte_range.end.0].to_string(), s.format.color)).collect()
    }

    /// The color of the first section that holds `needle`.
    fn color_of(src: &str, needle: &str) -> Color32 {
        colored(src).into_iter().find(|(t, _)| t.contains(needle)).unwrap_or_else(|| panic!("{needle} in {:?}", colored(src))).1
    }

    #[test]
    fn colors_keys_values_comments_and_items() {
        let src = "apiVersion: apps/v1  # the api\nspec:\n  replicas: 3\n  paused: false\n  image: \"nginx:1.2\"\n  args:\n    - --port=80\n    - name: web\n";
        let p = palette(true, TEXT);
        assert_eq!(color_of(src, "apiVersion"), p.key);
        assert_eq!(color_of(src, "apps/v1"), p.string);
        assert_eq!(color_of(src, "# the api"), p.comment);
        assert_eq!(color_of(src, "replicas"), p.key);
        assert_eq!(color_of(src, "3"), p.number);
        assert_eq!(color_of(src, "false"), p.keyword);
        assert_eq!(color_of(src, "\"nginx:1.2\""), p.string);
        assert_eq!(color_of(src, "--port=80"), p.string);
        assert_eq!(color_of(src, "name"), p.key);
        assert_eq!(color_of(src, "web"), p.string);
        assert!(colored(src).iter().any(|(t, c)| t.contains('-') && *c == p.punct));
        assert_eq!(color_of("v: 1.2.3", "1.2.3"), p.string, "a version is text");
        assert_eq!(color_of("v: 0x1F", "0x1F"), p.number);
        assert_eq!(color_of("# top\nk: v", "# top"), p.comment);
    }

    /// Every deeper line of a block scalar is text, whatever it looks like; a line back at the key's
    /// column ends it.
    #[test]
    fn block_scalars_are_text() {
        let src = "data:\n  app.properties: |\n    a: b\n    # not a comment\n\n    - not: an item\n  next: 1\n";
        let p = palette(true, TEXT);
        assert_eq!(color_of(src, "|"), p.keyword);
        for t in ["a: b", "# not a comment", "- not: an item"] {
            assert_eq!(color_of(src, t), p.string, "{t}");
        }
        assert_eq!(color_of(src, "next"), p.key);
        assert_eq!(color_of(src, "1"), p.number);
        // After `- key: |` the content belongs to the key's column, not to the dash.
        let src = "- run: |\n    echo hi\n  other: x\n";
        assert_eq!(color_of(src, "echo hi"), p.string);
        assert_eq!(color_of(src, "other"), p.key);
        // `- |` and `>-` with a comment.
        let src = "- |\n  text: here\nk: >-  # folded\n  more\nafter: 1\n";
        assert_eq!(color_of(src, "text: here"), p.string);
        assert_eq!(color_of(src, "# folded"), p.comment);
        assert_eq!(color_of(src, "more"), p.string);
        assert_eq!(color_of(src, "after"), p.key);
    }

    #[test]
    fn quotes_anchors_and_flow_collections() {
        let p = palette(true, TEXT);
        assert_eq!(color_of("\"a: b\": 1", "\"a: b\""), p.key, "a quoted key");
        assert_eq!(color_of("k: 'it''s: fine'", "'it''s: fine'"), p.string);
        assert_eq!(color_of("k: \"say \\\"hi\\\": ok\" # c", "say"), p.string);
        assert_eq!(color_of("k: \"say \\\"hi\\\": ok\" # c", "# c"), p.comment);
        assert_eq!(color_of("base: &b\n  x: 1\nuse: *b", "&b"), p.anchor);
        assert_eq!(color_of("t: !!str 5", "!!str"), p.anchor);
        let src = "labels: {app: web, tier: \"front\", n: 2, on: [a, true]}  # flow";
        assert_eq!(color_of(src, "app"), p.key);
        assert_eq!(color_of(src, "web"), p.string);
        assert_eq!(color_of(src, "\"front\""), p.string);
        assert_eq!(color_of(src, "2"), p.number);
        assert_eq!(color_of(src, "true"), p.keyword);
        assert_eq!(color_of(src, "# flow"), p.comment);
        assert_eq!(color_of("a: http://x:80/p", "http://x:80/p"), p.string, "a colon without a space is text");
        assert_eq!(color_of("---\na: 1", "---"), p.punct);
    }

    /// Whatever the input, the sections are the text, byte for byte, in order, on char boundaries.
    #[test]
    fn nothing_is_lost() {
        for src in [
            "",
            "\n",
            "k: v",
            "k: v\r\nj: \"é — 日本語\"\r\n",
            "  \t tab: after\n\t- x\n",
            "a: |\n  é\n\n  x",
            "k: 'unterminated\nj: \"also",
            "- - - deep\n-\n- :\n: x\n? q\n",
            "{[}]: #\n#",
            "k: [a, \"b\", {c: d}, \"e",
            "---\n...\n--- x\n",
            "k: &a !t v # c # d",
        ] {
            let j = job(src, &FontId::monospace(12.0), TEXT, false);
            assert_eq!(j.text, src, "{src:?}");
            let mut at = 0;
            for s in &j.sections {
                assert_eq!(s.byte_range.start.0, at, "{src:?}");
                assert!(s.byte_range.end.0 > at && j.text.is_char_boundary(s.byte_range.end.0), "{src:?}");
                at = s.byte_range.end.0;
            }
            assert_eq!(at, src.len(), "{src:?}");
        }
    }

    /// 1 MB of YAML in the time of a few frames (syntect needed ~9 s).
    #[test]
    #[ignore]
    fn bench_yaml_tokenizer() {
        let src: String = (0..4000).map(|i| format!("  key-{i}:\n    image: \"registry.example.com/team/app-{i}:1.{i}.0\"  # note\n    replicas: {i}\n    enabled: true\n    args: [--a, \"b\", {{c: {i}}}]\n    script: |\n      echo {i}\n      exit 0\n")).collect();
        let font = FontId::monospace(12.0);
        crate::prof::bench(&format!("yaml tokenizer, {} KB", src.len() / 1024), 5, || drop(job(&src, &font, TEXT, true)));
    }
}
