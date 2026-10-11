

use super::overview::*;
use super::*;

#[test]
fn avatar_initials() {
    assert_eq!(initials("K8S-QA"), "QA");
    assert_eq!(initials("rancher-desktop"), "RD");
    assert_eq!(initials("prod"), "PR");
    assert_eq!(initials("k3s"), "K3");
}

#[test]
fn sidebar_navigation() {
    let (pods, deps, nodes) = (kind_page("", "Pod"), kind_page("apps", "Deployment"), kind_page("", "Node"));
    assert_eq!(nav_target(&[(&pods, false)], &pods, false), NavTo::Focus(0)); // already open
    assert_eq!(nav_target(&[(&pods, false), (&deps, true)], &nodes, false), NavTo::Replace(1)); // reuse the preview
    assert_eq!(nav_target(&[(&pods, false), (&deps, true)], &nodes, true), NavTo::Push); // double-click: own tab
    assert_eq!(nav_target(&[(&pods, false)], &nodes, false), NavTo::Push); // no preview yet
    assert_eq!(nav_target(&[(&pods, false), (&deps, true)], &deps, true), NavTo::Focus(1)); // pinning the preview
}

#[test]
fn saved_tabs_round_trip() {
    let s = SavedTabs {
        pages: vec![SavedPage {
            page: kind_page("", "Pod"),
            preview: false,
            name: Some("web pods".into()),
            objs: vec![("".into(), "Pod".into(), "default".into(), "web-1".into())],
            active: 1,
            sections: vec![details::Section::Yaml],
            view: SavedView { search: "web".into(), re: true, sort: Some((crate::list::DCol::Cell(2), false)), status: Some("Running".into()), panel: Some(("default".into(), "web-2".into())) },
        }],
        cur: 0,
        ns: vec!["default".into()],
        wide: true,
    };
    assert_eq!(serde_json::from_str::<SavedTabs>(&serde_json::to_string(&s).unwrap()).unwrap(), s);
    // saves from before these fields still load
    let old = r#"{"pages":[{"page":"Overview","preview":true,"objs":[],"active":0}],"cur":0,"ns":[]}"#;
    assert_eq!(serde_json::from_str::<SavedTabs>(old).unwrap().pages[0].view, SavedView::default());
}

#[test]
fn workload_breakdown() {
    let mut d = ListData { cols: ["Name", "Ready"].map(|n| watch::Col { name: n.into(), ..Default::default() }).to_vec(), ..Default::default() };
    for (i, ready) in ["1/1", "0/1", "2/2"].iter().enumerate() {
        let r = Row { name: format!("d{i}"), namespace: "x".into(), uid: i.to_string(), rv: "1".into(), created: None, deleting: false, cells: vec![format!("d{i}"), ready.to_string()], images: vec![] };
        d.rows.insert(r.uid.clone(), Arc::new(r));
    }
    assert_eq!(breakdown(&d), vec![("Ready".to_string(), 2), ("Not ready".to_string(), 1)]);
}
