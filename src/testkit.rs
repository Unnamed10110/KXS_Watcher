//! Helpers for the egui tests: a headless frame loop fed with pointer and key events, and a client
//! that never connects (enough to build the tabs that hold one).

use egui::{Event, Key, Modifiers, PointerButton, Pos2, RawInput, Rect, Ui, Vec2};

use crate::tasks::Lock;

/// An egui context run frame by frame without a window. Each frame advances the clock by 50 ms, so a
/// press and its release are never the same instant (egui tells clicks and drags apart by time).
pub struct Harness {
    pub ctx: egui::Context,
    size: Vec2,
    frames: f64,
}

impl Harness {
    pub fn new(size: Vec2) -> Harness {
        Harness { ctx: egui::Context::default(), size, frames: 0.0 }
    }

    /// With the app's fonts and theme, for views that draw with its own text styles.
    pub fn themed(size: Vec2) -> Harness {
        let h = Harness::new(size);
        crate::theme::install_fonts(&h.ctx, "");
        crate::theme::apply(&h.ctx, crate::theme::Theme::Dark, crate::theme::Density::Comfortable, 13.5);
        h
    }

    /// One frame: `events` arrive, then `add` draws.
    pub fn frame(&mut self, events: Vec<Event>, add: impl FnMut(&mut Ui)) {
        self.frame_delay(events, add);
    }

    /// Same, returning how long egui may wait for the next frame (zero: something asked for it now).
    pub fn frame_delay(&mut self, events: Vec<Event>, add: impl FnMut(&mut Ui)) -> std::time::Duration {
        let out = self.frame_out(events, add);
        out.viewport_output.get(&egui::ViewportId::ROOT).map_or(std::time::Duration::MAX, |v| v.repaint_delay)
    }

    /// Same, returning everything the frame produced (the shapes it painted).
    pub fn frame_out(&mut self, events: Vec<Event>, add: impl FnMut(&mut Ui)) -> egui::FullOutput {
        self.frames += 1.0;
        let input = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, self.size)), events, time: Some(self.frames * 0.05), ..Default::default() };
        self.ctx.run_ui(input, add)
    }
}

/// The pointer moved to `p`.
pub fn moved(p: Pos2) -> Event {
    Event::PointerMoved(p)
}

/// The primary button went down (`true`) or up at `p`.
pub fn button(p: Pos2, pressed: bool) -> Event {
    Event::PointerButton { pos: p, button: PointerButton::Primary, pressed, modifiers: Modifiers::default() }
}

/// A key press with these modifiers.
pub fn key(key: Key, modifiers: Modifiers) -> Event {
    Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
}

/// A Kubernetes client pointed at a port nobody listens on: it builds, every request fails.
pub fn dummy_client() -> kube::Client {
    kube::Client::try_from(kube::Config::new("http://127.0.0.1:9".parse().unwrap())).unwrap()
}

/// A fake API server. Each request is answered with the next body queued for the first route whose
/// text is part of its URI (an empty body when that queue has run dry); `always` routes repeat.
#[derive(Clone, Default)]
pub struct MockApi {
    routes: std::sync::Arc<std::sync::Mutex<Vec<Route>>>,
    seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

struct Route {
    part: String,
    bodies: std::collections::VecDeque<String>,
    repeat: bool,
}

impl MockApi {
    /// Requests whose URI contains `part` get these bodies, one each, in order.
    pub fn route(self, part: &str, bodies: &[&str]) -> MockApi {
        self.routes.locked().push(Route { part: part.into(), bodies: bodies.iter().map(|b| b.to_string()).collect(), repeat: false });
        self
    }

    /// Requests whose URI contains `part` always get this body.
    pub fn always(self, part: &str, body: &str) -> MockApi {
        self.routes.locked().push(Route { part: part.into(), bodies: [body.to_string()].into(), repeat: true });
        self
    }

    pub fn client(&self) -> kube::Client {
        kube::Client::new(self.clone(), "default")
    }

    /// The URI of every request made so far.
    pub fn requests(&self) -> Vec<String> {
        self.seen.locked().clone()
    }
}

impl tower_service::Service<http::Request<kube::client::Body>> for MockApi {
    type Response = http::Response<kube::client::Body>;
    type Error = std::convert::Infallible;
    type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<kube::client::Body>) -> Self::Future {
        let uri = req.uri().to_string();
        self.seen.locked().push(uri.clone());
        let body = match self.routes.locked().iter_mut().find(|r| uri.contains(&r.part)) {
            Some(r) if r.repeat => r.bodies.front().cloned().unwrap_or_default(),
            Some(r) => r.bodies.pop_front().unwrap_or_default(),
            None => String::new(),
        };
        std::future::ready(Ok(http::Response::builder().status(200).body(kube::client::Body::from(body.into_bytes())).unwrap()))
    }
}
