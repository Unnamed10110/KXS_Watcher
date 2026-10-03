//! Drag selected text out of the window into another program (OLE drag source, text only).
//!
//! winit can only receive drags. A widget that sees the user dragging its selection asks with
//! `ctx.kxs_request_drag_out(text)` (vendored egui); `App::ui` then calls [`start`] at the end of
//! the frame, which blocks in `DoDragDrop` until the button is let go.

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::{
    DATA_S_SAMEFORMATETC, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, DV_E_FORMATETC, E_NOTIMPL, E_OUTOFMEMORY, GlobalFree, OLE_E_ADVISENOTSUPPORTED, S_OK,
};
use windows::Win32::System::Com::{
    DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{CF_UNICODETEXT, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, DoDragDrop, IDropSource, IDropSource_Impl, OleInitialize};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
use windows::Win32::UI::Shell::SHCreateStdEnumFmtEtc;
use windows::core::{BOOL, HRESULT, Ref, Result, implement};

/// The text as CF_UNICODETEXT wants it: UTF-16, CRLF line ends, a NUL at the end.
fn utf16(text: &str) -> Vec<u16> {
    let mut w: Vec<u16> = text.replace("\r\n", "\n").replace('\n', "\r\n").encode_utf16().collect();
    w.push(0);
    w
}

fn format() -> FORMATETC {
    FORMATETC { cfFormat: CF_UNICODETEXT.0, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 }
}

/// Whether the format asked for is the one offered: unicode text in global memory.
fn offered(f: *const FORMATETC) -> bool {
    // SAFETY: OLE passes a valid FORMATETC (or null, which `as_ref` turns into `None`).
    unsafe { f.as_ref() }.is_some_and(|f| f.cfFormat == CF_UNICODETEXT.0 && f.dwAspect == DVASPECT_CONTENT.0 && f.tymed & TYMED_HGLOBAL.0 as u32 != 0)
}

/// The text being dragged: the data object and the drop source in one.
#[implement(IDataObject, IDropSource)]
struct TextDrag(Vec<u16>);

impl IDataObject_Impl for TextDrag_Impl {
    fn GetData(&self, f: *const FORMATETC) -> Result<STGMEDIUM> {
        if !offered(f) {
            return Err(DV_E_FORMATETC.into()); // winit's own drop target asks for files first
        }
        // SAFETY: a fresh block per call, filled while locked; the receiver frees it
        // (`pUnkForRelease` is none), so it must not be kept or shared.
        unsafe {
            let h = GlobalAlloc(GMEM_MOVEABLE, self.0.len() * 2)?;
            let p = GlobalLock(h) as *mut u16;
            if p.is_null() {
                let _ = GlobalFree(Some(h));
                return Err(E_OUTOFMEMORY.into());
            }
            std::ptr::copy_nonoverlapping(self.0.as_ptr(), p, self.0.len());
            let _ = GlobalUnlock(h); // an error once the lock count reaches 0: expected
            Ok(STGMEDIUM { tymed: TYMED_HGLOBAL.0 as u32, u: STGMEDIUM_0 { hGlobal: h }, pUnkForRelease: ManuallyDrop::new(None) })
        }
    }

    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, f: *const FORMATETC) -> HRESULT {
        if offered(f) { S_OK } else { DV_E_FORMATETC }
    }

    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
        // SAFETY: `out` is a valid FORMATETC the caller asked us to fill.
        unsafe { (*out).ptd = std::ptr::null_mut() };
        DATA_S_SAMEFORMATETC
    }

    fn SetData(&self, _: *const FORMATETC, _: *const STGMEDIUM, _: BOOL) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    /// Word and Chromium (VS Code) list the formats before asking for one.
    fn EnumFormatEtc(&self, dir: u32) -> Result<IEnumFORMATETC> {
        if dir == DATADIR_GET.0 as u32 {
            // SAFETY: plain call with a one-element slice.
            unsafe { SHCreateStdEnumFmtEtc(&[format()]) }
        } else {
            Err(E_NOTIMPL.into())
        }
    }

    fn DAdvise(&self, _: *const FORMATETC, _: u32, _: Ref<IAdviseSink>) -> Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _: u32) -> Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

impl IDropSource_Impl for TextDrag_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool() {
            DRAGDROP_S_CANCEL
        } else if keys.0 & MK_LBUTTON.0 == 0 {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }

    fn GiveFeedback(&self, _: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// Drag `text` out until the button is let go; true if another program took it. Blocks, on the UI
/// thread (it has to be an OLE apartment): call it where no egui lock is held.
pub fn start(text: &str) -> bool {
    // SAFETY: plain OLE calls on the UI thread; the objects live for the call.
    unsafe {
        // The button is already up: the first `QueryContinueDrag` would drop at once.
        if GetAsyncKeyState(VK_LBUTTON.0 as i32) >= 0 {
            return false;
        }
        let _ = OleInitialize(None); // winit's drop target is off (see `main`): nobody did it yet
        let data: IDataObject = TextDrag(utf16(text)).into();
        let Ok(source) = windows::core::Interface::cast::<IDropSource>(&data) else { return false };
        let mut effect = DROPEFFECT_NONE;
        DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect) == DRAGDROP_S_DROP && effect != DROPEFFECT_NONE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Ole::CF_HDROP;

    #[test]
    fn text_goes_out_as_unicode_text() {
        assert_eq!(utf16("a\nb\r\nc"), "a\r\nb\r\nc\0".encode_utf16().collect::<Vec<_>>());
        let data: IDataObject = TextDrag(utf16("hola ñ\nok")).into();
        // SAFETY: in-process calls on an object made here; the block is read and freed.
        unsafe {
            assert!(data.QueryGetData(&format()) == S_OK);
            let mut other = format();
            other.cfFormat = CF_HDROP.0; // what winit's own drop target asks for
            assert!(data.QueryGetData(&other) != S_OK);
            assert!(data.GetData(&other).is_err());
            let m = data.GetData(&format()).unwrap();
            assert_eq!(m.tymed, TYMED_HGLOBAL.0 as u32);
            let h = m.u.hGlobal;
            let p = GlobalLock(h) as *const u16;
            let mut got = vec![];
            for i in 0.. {
                match *p.add(i) {
                    0 => break,
                    c => got.push(c),
                }
            }
            let _ = GlobalUnlock(h);
            let _ = GlobalFree(Some(h));
            assert_eq!(String::from_utf16(&got).unwrap(), "hola ñ\r\nok");
            let formats = data.EnumFormatEtc(DATADIR_GET.0 as u32).unwrap();
            let mut one = [FORMATETC::default()];
            assert_eq!(formats.Next(&mut one, None), S_OK);
            assert_eq!(one[0].cfFormat, CF_UNICODETEXT.0);
        }
    }

    #[test]
    fn escape_cancels_and_letting_go_drops() {
        let data: IDataObject = TextDrag(utf16("x")).into();
        let source: IDropSource = windows::core::Interface::cast(&data).unwrap();
        // SAFETY: in-process calls on an object made here.
        unsafe {
            assert_eq!(source.QueryContinueDrag(true.into(), MK_LBUTTON), DRAGDROP_S_CANCEL);
            assert_eq!(source.QueryContinueDrag(false.into(), MODIFIERKEYS_FLAGS(0)), DRAGDROP_S_DROP);
            assert_eq!(source.QueryContinueDrag(false.into(), MK_LBUTTON), S_OK);
        }
    }

    /// Labels (details panel) and text fields (YAML editor) keep their selection on a press inside
    /// it and ask to drag its text out when the pointer moves on; a click without moving collapses it.
    #[test]
    #[allow(unused_assignments)] // `st` is refreshed after every frame, read after some
    fn selections_of_labels_and_text_fields_drag_out() {
        use egui::{Event, PointerButton, Pos2, RawInput, Rect, vec2};
        let ctx = egui::Context::default();
        let mut field = String::from("one two three four five six");
        let mut k = 0.0;
        // One pass: the label's and the field's rects, and the field's selection.
        let mut frame = |events: Vec<Event>, field: &mut String| {
            let (mut label_rect, mut field_rect, mut field_cursor) = (Rect::ZERO, Rect::ZERO, None);
            k += 1.0;
            let input = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(600.0, 300.0))), events, time: Some(k * 0.05), ..Default::default() };
            let _ = ctx.run_ui(input, |ui| {
                label_rect = ui.label("alpha beta gamma delta epsilon").rect;
                let out = egui::TextEdit::singleline(field).desired_width(300.0).show(ui);
                (field_rect, field_cursor) = (out.response.rect, out.cursor_range);
            });
            (label_rect, field_rect, field_cursor)
        };
        let btn = |p: Pos2, pressed| Event::PointerButton { pos: p, button: PointerButton::Primary, pressed, modifiers: Default::default() };
        let label_text = |ctx: &egui::Context| ctx.plugin::<egui::text_selection::LabelSelectionState>().lock().selected_text();
        let mut st = frame(vec![], &mut field);
        st = frame(vec![], &mut field);

        // --- a label
        let y = st.0.center().y;
        let (a, b) = (Pos2::new(st.0.left() + 1.0, y), Pos2::new(st.0.left() + 80.0, y));
        for events in [vec![Event::PointerMoved(a)], vec![Event::PointerMoved(a)], vec![btn(a, true)]] {
            st = frame(events, &mut field);
        }
        for i in 1..=8 {
            st = frame(vec![Event::PointerMoved(a + (b - a) * (i as f32 / 8.0))], &mut field);
        }
        st = frame(vec![btn(b, false)], &mut field);
        st = frame(vec![], &mut field);
        let picked = label_text(&ctx);
        assert!(picked.len() > 4 && "alpha beta gamma delta epsilon".contains(&picked), "{picked:?}");
        let inside = Pos2::new(st.0.left() + 30.0, y);
        st = frame(vec![Event::PointerMoved(inside)], &mut field);
        st = frame(vec![btn(inside, true)], &mut field);
        assert!(ctx.kxs_take_drag_out().is_none() && label_text(&ctx) == picked, "a press keeps the selection: {:?} vs {picked:?}", label_text(&ctx));
        for i in 1..=4 {
            st = frame(vec![Event::PointerMoved(inside + vec2(5.0 * i as f32, 8.0 * i as f32))], &mut field);
        }
        assert_eq!(ctx.kxs_take_drag_out(), Some(picked.clone()));
        assert_eq!(label_text(&ctx), picked, "the drag does not change the selection");
        st = frame(vec![btn(inside + vec2(20.0, 32.0), false)], &mut field);
        // A click without moving collapses it.
        for events in [vec![Event::PointerMoved(inside)], vec![btn(inside, true)], vec![btn(inside, false)], vec![]] {
            st = frame(events, &mut field);
        }
        assert!(label_text(&ctx).is_empty(), "{:?}", label_text(&ctx));
        assert!(ctx.kxs_take_drag_out().is_none());

        // --- a text field
        let y = st.1.center().y;
        let (a, b) = (Pos2::new(st.1.left() + 6.0, y), Pos2::new(st.1.left() + 60.0, y));
        for events in [vec![Event::PointerMoved(a)], vec![btn(a, true)], vec![btn(a, false)]] {
            st = frame(events, &mut field); // focus it
        }
        for events in [vec![Event::PointerMoved(a)], vec![btn(a, true)]] {
            st = frame(events, &mut field);
        }
        for i in 1..=8 {
            st = frame(vec![Event::PointerMoved(a + (b - a) * (i as f32 / 8.0))], &mut field);
        }
        st = frame(vec![btn(b, false)], &mut field);
        st = frame(vec![], &mut field);
        let range = st.2.filter(|r| !r.is_empty()).expect("selected");
        let picked = range.slice_str(&field).to_owned();
        assert!(picked.len() > 3, "{picked:?}");
        let inside = Pos2::new(st.1.left() + 30.0, y);
        st = frame(vec![Event::PointerMoved(inside)], &mut field);
        st = frame(vec![btn(inside, true)], &mut field);
        assert!(ctx.kxs_take_drag_out().is_none());
        assert_eq!(st.2.map(|r| r.slice_str(&field).to_owned()), Some(picked.clone()), "a press keeps the selection");
        for i in 1..=4 {
            st = frame(vec![Event::PointerMoved(inside + vec2(5.0 * i as f32, 8.0 * i as f32))], &mut field);
        }
        assert_eq!(ctx.kxs_take_drag_out(), Some(picked.clone()));
        st = frame(vec![btn(inside + vec2(20.0, 32.0), false)], &mut field);
        for events in [vec![Event::PointerMoved(inside)], vec![btn(inside, true)], vec![btn(inside, false)], vec![]] {
            st = frame(events, &mut field);
        }
        assert!(st.2.is_some_and(|r| r.is_empty()), "a click collapses it: {:?}", st.2);
        assert_eq!(field, "one two three four five six", "nothing was edited");
    }
}
