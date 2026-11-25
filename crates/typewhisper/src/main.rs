use adw::prelude::*;

const APP_ID: &str = "com.typewhisper.TypeWhisper";

fn main() -> gtk::glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_window);
    app.run()
}

fn build_window(app: &adw::Application) {
    let page = adw::StatusPage::builder()
        .icon_name("audio-input-microphone-symbolic")
        .title("TypeWhisper")
        .description("Dictation is not wired up yet. This window is the starting point of the app.")
        .build();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&page));
    adw::ApplicationWindow::builder()
        .application(app)
        .title("TypeWhisper")
        .default_width(520)
        .default_height(420)
        .content(&view)
        .build()
        .present();
}
