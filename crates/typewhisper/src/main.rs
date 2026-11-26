mod app;
mod cloud;
mod formatting;
mod kwin_script;
mod overlay;
mod server_profiles;
mod shell_bridge;
mod terminals;
mod waveform;

use adw::prelude::*;

pub const APP_ID: &str = "com.typewhisper.TypeWhisper";

fn main() -> gtk::glib::ExitCode {
    tw_platform::i18n::init();
    let application = adw::Application::builder().application_id(APP_ID).build();
    application.connect_startup(app::startup);
    application.connect_activate(app::activate);
    application.run()
}
