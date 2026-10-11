//! What a cluster tab shows: page tabs (list, overview, helm...), object tabs, and what is saved of them.

use super::*;

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub(super) enum Page {
    Overview,
    Workloads,
    Kind(String, String),
    Forwards,
    Helm,
    /// Text search inside Config Maps and Secrets.
    Search,
    /// A new tab (Ctrl+T): pick what it shows.
    Launcher,
}

pub(super) fn kind_page(g: &str, k: &str) -> Page {
    Page::Kind(g.into(), k.into())
}

/// Open tabs of a cluster, saved across restarts.
#[derive(Clone, Default, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedTabs {
    pub(super) pages: Vec<SavedPage>,
    pub(super) cur: usize,
    pub(super) ns: Vec<String>,
    #[serde(default)]
    pub(super) wide: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct SavedPage {
    pub(super) page: Page,
    pub(super) preview: bool,
    /// Name given to the tab.
    #[serde(default)]
    pub(super) name: Option<String>,
    /// Object tabs: (group, kind, namespace, name).
    pub(super) objs: Vec<(String, String, String, String)>,
    pub(super) active: usize,
    /// What each object tab shows, in `objs` order.
    #[serde(default)]
    pub(super) sections: Vec<details::Section>,
    #[serde(default)]
    pub(super) view: SavedView,
}

/// A page's view: the list's filter, sort and pods status, and the row whose details are open;
/// the Search page's query.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct SavedView {
    pub(super) search: String,
    pub(super) re: bool,
    pub(super) sort: Option<(crate::list::DCol, bool)>,
    pub(super) status: Option<String>,
    /// Details panel: (namespace, name).
    pub(super) panel: Option<(String, String)>,
}

/// A page tab (Pods, Deployments, Overview…) with its own list and the object tabs opened from it.
pub(super) struct PageTab {
    pub(super) id: u64,
    pub(super) page: Page,
    /// Replaced by the next single click in the sidebar (shown in italics).
    pub(super) preview: bool,
    pub(super) list: Option<List>,
    pub(super) workloads: Vec<List>,
    /// Object tabs; `active` 0 is the page view, i > 0 is `subs[i - 1]`.
    pub(super) subs: Vec<ObjTab>,
    pub(super) active: usize,
    /// Details of the clicked row, right of the list.
    pub(super) panel: Option<ObjTab>,
    pub(super) content: Option<ContentSearch>,
    /// New tab: its filter.
    pub(super) launch_query: String,
    /// Name given to the tab (rename); the page's own title when `None`.
    pub(super) name: Option<String>,
    /// Details panel open at exit: (namespace, name), shown once the list has loaded.
    pub(super) panel_restore: Option<(String, String)>,
}

impl PageTab {
    pub(super) fn new(page: Page, preview: bool) -> Self {
        PageTab { id: next_id(), page, preview, list: None, workloads: vec![], subs: vec![], active: 0, panel: None, content: None, launch_query: String::new(), name: None, panel_restore: None }
    }

    pub(super) fn view(&self) -> SavedView {
        let mut v = SavedView::default();
        if let Some(l) = &self.list {
            (v.search, v.re, v.sort, v.status) = (l.search.clone(), l.filter_re, l.sort, l.status.map(String::from));
            v.panel = self.panel.as_ref().map(|p| (p.details.t.ns.clone(), p.details.t.name.clone())).or_else(|| self.panel_restore.clone());
        }
        if let Some(c) = &self.content {
            (v.search, v.re) = (c.find.query.clone(), c.find.regex);
        }
        v
    }

    pub(super) fn set_view(&mut self, v: SavedView) {
        if let Some(l) = &mut self.list {
            (l.search, l.filter_re, l.sort) = (v.search.clone(), v.re, v.sort);
            l.status = v.status.and_then(|s| crate::list::POD_BUCKETS.into_iter().find(|b| *b == s));
            self.panel_restore = v.panel;
        }
        if let Some(c) = &mut self.content {
            (c.find.query, c.find.regex) = (v.search, v.re);
        }
    }

    /// (Re)start the watches this page needs; also after a namespace change.
    pub(super) fn load(&mut self, ctx: &egui::Context, client: &Client, kinds: &[Kind], ns_sel: &BTreeSet<String>) {
        let kind = |g: &str, k: &str| kinds.iter().find(|x| x.is(g, k)).cloned();
        let keep = self.list.take().map(|l| (l.kind.ar.clone(), l.search, l.filter_re, l.sort));
        self.panel = None;
        self.workloads.clear();
        match &self.page {
            Page::Kind(g, k) => {
                if let Some(kd) = kind(g, k) {
                    let mut l = List::new(ctx, client, kd, ns_sel, None);
                    if let Some((_, search, re, sort)) = keep.filter(|(ar, ..)| *ar == l.kind.ar) {
                        (l.search, l.filter_re, l.sort) = (search, re, sort); // namespace change: keep the user's view
                    }
                    self.list = Some(l);
                }
            }
            Page::Overview => {} // uses the cluster's warning watch
            Page::Workloads => self.workloads = WORKLOADS.iter().filter_map(|(g, k)| kind(g, k)).map(|k| List::new(ctx, client, k, ns_sel, None)).collect(),
            Page::Search => {
                let ns = ns_sel.iter().cloned().collect();
                match &mut self.content {
                    Some(c) => c.reload(ctx, Some(ns)), // keeps the query
                    None => self.content = Some(ContentSearch::new(ctx, client.clone(), ns)),
                }
            }
            Page::Helm | Page::Forwards | Page::Launcher => {}
        }
    }

    pub(super) fn title(&self) -> String {
        if let Some(n) = &self.name {
            return n.clone();
        }
        match &self.page {
            Page::Overview => "Overview".into(),
            Page::Workloads => "Workloads".into(),
            Page::Forwards => "Port Forwarding".into(),
            Page::Helm => "Helm releases".into(),
            Page::Search => "Search".into(),
            Page::Launcher => "New tab".into(),
            Page::Kind(_, k) => self.list.as_ref().map_or(k.clone(), |l| label(&l.kind)),
        }
    }

    /// Open (or focus) an object tab here; content makes the page tab permanent.
    pub(super) fn open_obj(&mut self, ctx: &egui::Context, client: &Client, kind: Kind, ns: String, name: String, focus: bool) {
        self.preview = false;
        let ns = if kind.namespaced { ns } else { String::new() };
        if let Some(i) = self.subs.iter().position(|t| t.details.t.kind.ar == kind.ar && t.details.t.ns == ns && t.details.t.name == name) {
            if focus {
                self.active = i + 1;
            }
            return;
        }
        let data: Shared = Default::default();
        let watch = (kind.can("watch") && kind.can("list")).then(|| {
            let scope = (!ns.is_empty()).then(|| ns.clone());
            Bg::spawn(watch::run(client.clone(), kind.ar.clone(), scope, Some(format!("metadata.name={name}")), data.clone(), ctx.clone()))
        });
        let title = format!("{}/{name}", kind.ar.kind.to_lowercase());
        let details = Details::new(ctx, client.clone(), kind, ns, name);
        self.subs.push(ObjTab { id: next_id(), title, details, data, uid: None, _watch: watch });
        if focus {
            self.active = self.subs.len();
        }
    }
}

/// An object shown in an object tab (own single-object watch, `uid: None`) or in the details panel
/// (follows the list's row `uid`, no extra watch). Either way the data only says when to refresh.
pub(super) struct ObjTab {
    pub(super) id: u64,
    pub(super) title: String,
    pub(super) details: Details,
    pub(super) data: Shared,
    pub(super) uid: Option<String>,
    pub(super) _watch: Option<Bg>,
}

impl ObjTab {
    pub(super) fn sync(&mut self, ctx: &egui::Context) {
        let d = self.data.locked();
        d.shown.mark(); // object tabs and the details panel are few, and they follow the object live
        if d.synced {
            let row = match &self.uid {
                Some(u) => d.rows.get(u),
                None => d.rows.values().next(),
            };
            let rv = row.map(|r| r.rv.clone());
            drop(d);
            self.details.sync(ctx, rv.as_deref());
        }
    }
}

#[derive(Debug, PartialEq)]
pub(super) enum NavTo {
    Focus(usize),
    Replace(usize),
    Push,
}

/// Where a sidebar click goes: an open page is focused; a single click reuses the preview tab.
pub(super) fn nav_target(tabs: &[(&Page, bool)], page: &Page, pin: bool) -> NavTo {
    if let Some(i) = tabs.iter().position(|(p, _)| *p == page) {
        return NavTo::Focus(i);
    }
    match tabs.iter().position(|(_, preview)| *preview) {
        Some(i) if !pin => NavTo::Replace(i),
        _ => NavTo::Push,
    }
}

/// What a page view asks for after drawing.
#[derive(Default)]
pub(super) struct PageEv {
    pub(super) ns_changed: bool,
    pub(super) goto: Option<Page>,
    /// The new tab picked what it shows.
    pub(super) replace: Option<Page>,
}
