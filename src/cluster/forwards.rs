//! The port forwarding page.

use super::*;

impl Ready {
    pub(super) fn forwards_ui(&mut self, ui: &mut Ui) {
        ui.heading("Port Forwarding");
        if self.forwards.is_empty() {
            ui.label("No active port forwards. Start one from the ports of a Pod or Service in its details.");
            return;
        }
        let mut stop = None;
        egui::Grid::new("forwards").num_columns(6).striped(true).spacing([16.0, 6.0]).show(ui, |ui| {
            for h in ["Target", "Namespace", "Remote", "Local", "Status", ""] {
                ui.label(RichText::new(h).weak());
            }
            ui.end_row();
            for (i, f) in self.forwards.iter().enumerate() {
                let local = *f.local.locked();
                ui.label(&f.target);
                ui.label(&f.ns);
                ui.label(f.remote.to_string());
                match local {
                    Some(p) => ui.hyperlink_to(format!("localhost:{p}"), format!("http://localhost:{p}")),
                    None => ui.label("—"),
                };
                let status = f.status.locked().clone();
                ui.colored_label(if local.is_some() { GREEN } else { RED }, status);
                if ui.button("■ Stop").clicked() {
                    stop = Some(i);
                }
                ui.end_row();
            }
        });
        if let Some(i) = stop {
            self.forwards.remove(i); // drop kills kubectl
        }
    }
}
