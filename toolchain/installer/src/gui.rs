//! Minimal GTK4 installer front end (optional `gtk` feature).

use anyhow::Result;

use crate::config::InstallConfig;

pub fn run_gui(_config: InstallConfig) -> Result<()> {
    use gtk4::prelude::*;

    let app = gtk4::Application::builder()
        .application_id("org.evergreen.Installer")
        .build();

    app.connect_activate(|app| {
        let window = gtk4::ApplicationWindow::new(app);
        window.set_title(Some("Evergreen Linux Installer"));
        window.set_default_size(640, 420);

        let box_ = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        box_.set_margin_top(24);
        box_.set_margin_bottom(24);
        box_.set_margin_start(24);
        box_.set_margin_end(24);

        let label = gtk4::Label::new(Some(
            "Evergreen Linux installer\n\nUse the text mode (--tui) or a configuration file (--config) to perform an installation.",
        ));
        label.set_wrap(true);
        box_.append(&label);

        let button = gtk4::Button::with_label("Close");
        button.connect_clicked(move |_| {});
        box_.append(&button);

        window.set_child(Some(&box_));
        window.present();
    });

    app.run();
    Ok(())
}
