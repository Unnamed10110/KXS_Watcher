//! A tab (cluster view, logs, terminal, editor) and what tabs produce during a frame.

use super::*;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

pub struct Tab {
    pub id: u64,
    pub body: Body,
    /// Context id of the cluster the tab belongs to (for its accent color).
    pub cluster: Option<String>,
}

pub enum Body {
    Cluster(Box<ClusterTab>),
    Logs(Box<LogTab>),
    Term(Box<TermTab>),
    Yaml(Box<YamlTab>),
}

/// What tabs produce during a frame: new dock tabs and notifications.
#[derive(Default)]
pub struct Out {
    pub(crate) tabs: Vec<Tab>,
    pub(crate) toasts: Vec<(String, bool)>,
    /// Image for node-shell pods (from Settings).
    pub node_image: String,
}

impl Out {
    pub fn toast(&mut self, r: ops::Res) {
        self.toasts.push(match r {
            Ok(m) => (m, false),
            Err(e) => (e, true),
        });
    }

    pub fn tab(&mut self, body: Body) {
        self.tabs.push(Tab { id: next_id(), body, cluster: None });
    }

    /// A local terminal pinned to a context; reopened on the next start.
    pub fn local_term(&mut self, ctx: &egui::Context, k: &Ctx) {
        self.term(ctx, format!("Terminal {}", k.name), tabs::local_shell(k), None);
        if let Some(Tab { body: Body::Term(t), .. }) = self.tabs.last_mut() {
            t.local = Some(k.id());
        }
    }

    /// Terminal tab; `cleanup` names a pod to delete when it closes (node shells).
    pub fn term(&mut self, ctx: &egui::Context, title: String, cmd: tabs::Command, cleanup: Option<tabs::Cleanup>) {
        let id = next_id();
        match TermTab::new(ctx, id, title, cmd) {
            Ok(mut t) => {
                t.cleanup = cleanup;
                self.tabs.push(Tab { id, body: Body::Term(Box::new(t)), cluster: None });
            }
            Err(e) => {
                self.toasts.push((format!("Cannot start terminal: {e}"), true));
                if let Some((client, ns, pod)) = cleanup {
                    tokio::spawn(ops::delete(client, kube::api::ApiResource::erase::<k8s_openapi::api::core::v1::Pod>(&()), ns, pod));
                }
            }
        }
    }
}

impl Tab {
    /// Ctrl+K opens the find bar; Ctrl+F (`filter`) focuses a list's filter box. False for terminals.
    pub(crate) fn open_find(&mut self, filter: bool) -> bool {
        match &mut self.body {
            Body::Cluster(c) => c.open_find(filter),
            Body::Logs(l) => l.find.open(),
            Body::Yaml(y) => y.find.open(),
            Body::Term(_) => return false,
        }
        true
    }
}
