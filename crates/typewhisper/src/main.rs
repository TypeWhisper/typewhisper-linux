mod app;
mod formatting;
mod overlay;
mod shell_bridge;

use adw::prelude::*;

pub const APP_ID: &str = "com.typewhisper.TypeWhisper";

fn main() -> gtk::glib::ExitCode {
    let application = adw::Application::builder().application_id(APP_ID).build();
    application.connect_startup(app::startup);
    application.connect_activate(app::activate);
    application.run()
}
