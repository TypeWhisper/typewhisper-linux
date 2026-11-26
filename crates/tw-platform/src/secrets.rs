//! Secrets of plugins, such as API keys, in the desktop's keyring.
//!
//! Outside Flatpak the keyring is the Secret Service on the session bus:
//! GNOME Keyring, or KWallet on KDE Plasma. Inside Flatpak, oo7 keeps the
//! secrets in a file in the app's data folder, encrypted with a key that the
//! Secret portal hands out, so the sandbox needs no access to the Secret Service.
//!
//! Each secret belongs to a plugin ID and an entry name, as on macOS, where
//! the Keychain service is `<plugin ID>.<entry>`: `com.typewhisper.groq` and
//! `api-key`, or one entry per server for a plugin with several. Secrets never
//! go into `settings.json`.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// The `xdg:schema` of every item, so the items are told apart from those of
/// other apps.
pub const SCHEMA: &str = "com.typewhisper.TypeWhisper.PluginSecret";
const PLUGIN_ATTRIBUTE: &str = "plugin";
const ENTRY_ATTRIBUTE: &str = "entry";

#[derive(Debug)]
pub struct SecretError(String);

impl std::fmt::Display for SecretError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SecretError {}

impl From<oo7::Error> for SecretError {
    fn from(error: oo7::Error) -> Self {
        SecretError(error.to_string())
    }
}

pub struct SecretStore {
    backend: Backend,
}

enum Backend {
    Keyring(oo7::Keyring),
    /// For tests: secrets by plugin ID and entry, gone when the store is.
    Memory(Mutex<BTreeMap<(String, String), String>>),
}

impl SecretStore {
    /// Opens the desktop's keyring.
    pub async fn open() -> Result<Self, SecretError> {
        Ok(Self {
            backend: Backend::Keyring(oo7::Keyring::new().await?),
        })
    }

    /// A store that keeps its secrets in memory only, for tests.
    pub fn in_memory() -> Self {
        Self {
            backend: Backend::Memory(Mutex::default()),
        }
    }

    /// Saves `secret`, replacing the one before. A blank one removes the entry,
    /// as on macOS. Spaces around the secret are dropped.
    pub async fn store(
        &self,
        plugin_id: &str,
        entry: &str,
        secret: &str,
    ) -> Result<(), SecretError> {
        let secret = secret.trim();
        if secret.is_empty() {
            return self.delete(plugin_id, entry).await;
        }
        match &self.backend {
            Backend::Keyring(keyring) => {
                unlock(keyring).await?;
                keyring
                    .create_item(
                        &label(plugin_id, entry),
                        &attributes(plugin_id, entry),
                        oo7::Secret::text(secret),
                        true,
                    )
                    .await?;
            }
            Backend::Memory(secrets) => {
                memory(secrets).insert((plugin_id.into(), entry.into()), secret.into());
            }
        }
        Ok(())
    }

    /// The secret of one entry, if there is one.
    pub async fn load(&self, plugin_id: &str, entry: &str) -> Result<Option<String>, SecretError> {
        Ok(self.load_all(plugin_id).await?.remove(entry))
    }

    /// Every secret of the plugin, by entry name.
    pub async fn load_all(&self, plugin_id: &str) -> Result<BTreeMap<String, String>, SecretError> {
        match &self.backend {
            Backend::Keyring(keyring) => {
                unlock(keyring).await?;
                let mut found = BTreeMap::new();
                for item in keyring.search_items(&search(plugin_id)).await? {
                    if item.is_locked().await? {
                        item.unlock().await?;
                    }
                    let Some(entry) = item.attributes().await?.remove(ENTRY_ATTRIBUTE) else {
                        continue;
                    };
                    let secret = item.secret().await?;
                    let Ok(text) = std::str::from_utf8(&secret) else {
                        continue;
                    };
                    found.insert(entry, text.to_owned());
                }
                Ok(found)
            }
            Backend::Memory(secrets) => Ok(memory(secrets)
                .iter()
                .filter(|((plugin, _), _)| plugin == plugin_id)
                .map(|((_, entry), secret)| (entry.clone(), secret.clone()))
                .collect()),
        }
    }

    /// Removes one entry. Removing a missing entry is no error.
    pub async fn delete(&self, plugin_id: &str, entry: &str) -> Result<(), SecretError> {
        match &self.backend {
            Backend::Keyring(keyring) => {
                unlock(keyring).await?;
                keyring.delete(&attributes(plugin_id, entry)).await?;
            }
            Backend::Memory(secrets) => {
                memory(secrets).remove(&(plugin_id.to_owned(), entry.to_owned()));
            }
        }
        Ok(())
    }
}

/// A locked keyring asks the user for its password; the login keyring is
/// usually unlocked already.
async fn unlock(keyring: &oo7::Keyring) -> Result<(), SecretError> {
    if keyring.is_locked().await? {
        keyring.unlock().await?;
    }
    Ok(())
}

fn memory(
    secrets: &Mutex<BTreeMap<(String, String), String>>,
) -> std::sync::MutexGuard<'_, BTreeMap<(String, String), String>> {
    secrets
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What a keyring app such as Seahorse shows, the macOS Keychain service name
/// after the app's name.
fn label(plugin_id: &str, entry: &str) -> String {
    format!("TypeWhisper {plugin_id}.{entry}")
}

fn attributes<'a>(plugin_id: &'a str, entry: &'a str) -> [(&'static str, &'a str); 3] {
    [
        (oo7::XDG_SCHEMA_ATTRIBUTE, SCHEMA),
        (PLUGIN_ATTRIBUTE, plugin_id),
        (ENTRY_ATTRIBUTE, entry),
    ]
}

fn search(plugin_id: &str) -> [(&'static str, &str); 2] {
    [
        (oo7::XDG_SCHEMA_ATTRIBUTE, SCHEMA),
        (PLUGIN_ATTRIBUTE, plugin_id),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const GROQ: &str = "com.typewhisper.groq";
    /// Not a real plugin, so the test on the desktop keyring leaves real keys alone.
    const TEST_PLUGIN: &str = "com.typewhisper.secret-store-test";
    const OTHER_PLUGIN: &str = "com.typewhisper.secret-store-test-other";

    fn round_trip(store: &SecretStore) {
        async_io::block_on(async {
            store
                .store(TEST_PLUGIN, "api-key", "  gsk_one \n")
                .await
                .unwrap();
            store.store(TEST_PLUGIN, "server-2", "two").await.unwrap();
            store.store(OTHER_PLUGIN, "api-key", "other").await.unwrap();
            assert_eq!(
                store.load(TEST_PLUGIN, "api-key").await.unwrap().as_deref(),
                Some("gsk_one")
            );
            let all = store.load_all(TEST_PLUGIN).await.unwrap();
            assert_eq!(all.keys().collect::<Vec<_>>(), ["api-key", "server-2"]);

            store
                .store(TEST_PLUGIN, "api-key", "gsk_new")
                .await
                .unwrap();
            assert_eq!(
                store.load(TEST_PLUGIN, "api-key").await.unwrap().as_deref(),
                Some("gsk_new")
            );
            // A blank secret removes the entry, and removing it twice is fine.
            store.store(TEST_PLUGIN, "api-key", "   ").await.unwrap();
            assert_eq!(store.load(TEST_PLUGIN, "api-key").await.unwrap(), None);
            store.delete(TEST_PLUGIN, "api-key").await.unwrap();
            store.delete(TEST_PLUGIN, "server-2").await.unwrap();
            store.delete(OTHER_PLUGIN, "api-key").await.unwrap();
            assert!(store.load_all(TEST_PLUGIN).await.unwrap().is_empty());
        });
    }

    #[test]
    fn keeps_secrets_per_plugin_and_entry() {
        round_trip(&SecretStore::in_memory());
    }

    #[test]
    fn names_items_by_plugin_and_entry() {
        assert_eq!(
            attributes(GROQ, "api-key"),
            [
                ("xdg:schema", SCHEMA),
                ("plugin", GROQ),
                ("entry", "api-key")
            ]
        );
        // The search for a plugin's secrets is the item's attributes without the entry.
        assert_eq!(search(GROQ), attributes(GROQ, "api-key")[..2]);
        assert_eq!(
            label(GROQ, "api-key"),
            "TypeWhisper com.typewhisper.groq.api-key"
        );
    }

    /// Writes to the real keyring, so it runs only on request, in a desktop
    /// session with an unlocked keyring:
    /// `cargo test -p tw-platform -- --ignored desktop_keyring`.
    #[test]
    #[ignore = "needs the desktop's keyring"]
    fn desktop_keyring() {
        let store = async_io::block_on(SecretStore::open()).unwrap();
        round_trip(&store);
    }
}
