//! A D-Bus connection for portal calls that carries TypeWhisper's app ID.

use std::collections::HashMap;

use ashpd::zbus;

/// Opens a session bus connection and, for apps outside Flatpak, registers the
/// app ID with the portal. Portals identify host apps per connection, and the
/// consent dialogs show the name from the matching `.desktop` file.
pub async fn connection(app_id: &str) -> zbus::Result<zbus::Connection> {
    let connection = zbus::Connection::session().await?;
    if std::env::var_os("FLATPAK_ID").is_none() {
        let options: HashMap<&str, zbus::Variant<'_>> = HashMap::new();
        // Fails when the portal already knows the app (for example from its
        // systemd scope) or is too old to have the registry; both are fine.
        if let Err(error) = connection
            .call_method(
                Some("org.freedesktop.portal.Desktop"),
                "/org/freedesktop/portal/desktop",
                Some("org.freedesktop.host.portal.Registry"),
                "Register",
                &(app_id, options),
            )
            .await
        {
            eprintln!("typewhisper: portal registry: {error}");
        }
    }
    Ok(connection)
}
