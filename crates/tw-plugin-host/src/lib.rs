//! Finds installed plugins and runs each as its own process.
//!
//! Plugins live in one folder each, with `manifest.json` at the root. They come
//! from three places, searched in this order:
//!
//! 1. folders in `$TYPEWHISPER_PLUGIN_PATH` (colon-separated, for development),
//! 2. the user's plugin folder, filled by the in-app catalog,
//! 3. system folders: Flatpak extensions under `/app/plugins`, distribution
//!    packages under `/usr/lib/typewhisper/plugins`.
//!
//! For the same plugin ID the highest version wins; on a tie a system copy wins.

use std::cmp::Ordering;
use std::io::{self, BufReader, BufWriter};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};
use tw_plugin_protocol::{Frame, Manifest, PROTOCOL_VERSION, read_frame, write_frame};

pub const SYSTEM_PLUGIN_DIRS: &[&str] = &[
    "/app/plugins",
    "/usr/lib/typewhisper/plugins",
    "/usr/local/lib/typewhisper/plugins",
];

#[derive(Debug, Clone)]
pub struct InstalledPlugin {
    pub manifest: Manifest,
    pub dir: PathBuf,
    pub is_system: bool,
}

impl InstalledPlugin {
    pub fn executable(&self) -> PathBuf {
        self.dir.join(&self.manifest.executable)
    }
}

/// Search folders: development folders first, then the user folder, then system folders.
pub fn plugin_dirs(user_dir: &Path) -> Vec<(PathBuf, bool)> {
    let mut dirs: Vec<(PathBuf, bool)> = std::env::var("TYPEWHISPER_PLUGIN_PATH")
        .ok()
        .map(|value| {
            value
                .split(':')
                .filter(|s| !s.is_empty())
                .map(|s| (PathBuf::from(s), false))
                .collect()
        })
        .unwrap_or_default();
    dirs.push((user_dir.to_path_buf(), false));
    dirs.extend(SYSTEM_PLUGIN_DIRS.iter().map(|d| (PathBuf::from(d), true)));
    dirs
}

/// Reads every `*/manifest.json` below the given folders.
pub fn discover(dirs: &[(PathBuf, bool)]) -> Vec<InstalledPlugin> {
    let mut found: Vec<InstalledPlugin> = Vec::new();
    for (root, is_system) in dirs {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            let Ok(text) = std::fs::read_to_string(dir.join("manifest.json")) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_str::<Manifest>(&text) else {
                continue;
            };
            if manifest.protocol_version != PROTOCOL_VERSION || !is_inside(&manifest.executable) {
                continue;
            }
            let candidate = InstalledPlugin {
                manifest,
                dir,
                is_system: *is_system,
            };
            match found
                .iter_mut()
                .find(|p| p.manifest.id == candidate.manifest.id)
            {
                Some(existing) if prefer(&candidate, existing) => *existing = candidate,
                Some(_) => {}
                None => found.push(candidate),
            }
        }
    }
    found.sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));
    found
}

/// A relative path that cannot leave the plugin folder.
fn is_inside(path: &str) -> bool {
    let path = Path::new(path);
    !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}

fn prefer(candidate: &InstalledPlugin, existing: &InstalledPlugin) -> bool {
    match compare_versions(&candidate.manifest.version, &existing.manifest.version) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => candidate.is_system && !existing.is_system,
    }
}

/// Compares dotted numeric versions; missing or non-numeric parts count as 0.
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let parse = |v: &str| {
        v.split('.')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    let (a, b) = (parse(a), parse(b));
    for i in 0..a.len().max(b.len()) {
        match a.get(i).unwrap_or(&0).cmp(b.get(i).unwrap_or(&0)) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    Ordering::Equal
}

#[derive(Debug)]
pub enum PluginError {
    Io(io::Error),
    /// The plugin answered with a JSON-RPC error.
    Plugin {
        code: i64,
        message: String,
    },
    /// The plugin closed its output or sent something that is not a response.
    Protocol(String),
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PluginError::Io(e) => write!(f, "plugin process: {e}"),
            PluginError::Plugin { message, .. } => write!(f, "{message}"),
            PluginError::Protocol(m) => write!(f, "plugin protocol: {m}"),
        }
    }
}

impl From<io::Error> for PluginError {
    fn from(e: io::Error) -> Self {
        PluginError::Io(e)
    }
}

/// A running plugin. Calls block, so use it from a worker thread.
pub struct PluginProcess {
    child: Child,
    input: BufWriter<ChildStdin>,
    output: BufReader<ChildStdout>,
    next_id: u64,
}

impl PluginProcess {
    /// Starts the plugin and performs `initialize`. Returns the process and the
    /// plugin's answer (capabilities, models).
    pub fn start(
        plugin: &InstalledPlugin,
        data_dir: &Path,
        host_version: &str,
    ) -> Result<(Self, Value), PluginError> {
        std::fs::create_dir_all(data_dir)?;
        // A relative program path would be resolved after the change of directory.
        let executable = std::path::absolute(plugin.executable())?;
        let mut child = Command::new(executable)
            .current_dir(&plugin.dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let input = BufWriter::new(child.stdin.take().expect("piped stdin"));
        let output = BufReader::new(child.stdout.take().expect("piped stdout"));
        let mut process = Self {
            child,
            input,
            output,
            next_id: 0,
        };
        let info = process.call(
            "initialize",
            json!({"protocolVersion": PROTOCOL_VERSION, "hostVersion": host_version, "dataDir": data_dir}),
            &mut |_, _| {},
        )?;
        Ok((process, info))
    }

    /// Sends a request and waits for its response. Notifications that arrive in
    /// between (for example download progress) go to `on_notification`.
    pub fn call(
        &mut self,
        method: &str,
        params: Value,
        on_notification: &mut dyn FnMut(&str, &Value),
    ) -> Result<Value, PluginError> {
        self.next_id += 1;
        let id = self.next_id;
        write_frame(
            &mut self.input,
            &Frame::Json(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})),
        )?;
        loop {
            match read_frame(&mut self.output)? {
                None => {
                    return Err(PluginError::Protocol(format!(
                        "plugin exited during {method}"
                    )));
                }
                Some(Frame::Pcm(_)) => {
                    return Err(PluginError::Protocol("unexpected audio from plugin".into()));
                }
                Some(Frame::Json(message)) => {
                    if message.get("id").and_then(Value::as_u64) == Some(id) {
                        if let Some(error) = message.get("error") {
                            return Err(PluginError::Plugin {
                                code: error["code"].as_i64().unwrap_or(0),
                                message: error["message"]
                                    .as_str()
                                    .unwrap_or("plugin error")
                                    .to_owned(),
                            });
                        }
                        return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                    }
                    if let Some(name) = message.get("method").and_then(Value::as_str) {
                        on_notification(name, message.get("params").unwrap_or(&Value::Null));
                    }
                }
            }
        }
    }

    /// Transcribes 16 kHz mono audio with the loaded model.
    pub fn transcribe(
        &mut self,
        samples: &[f32],
        language: Option<&str>,
    ) -> Result<Value, PluginError> {
        self.call(
            "transcription.begin",
            json!({"requestId": "dictation", "language": language}),
            &mut |_, _| {},
        )?;
        for chunk in samples.chunks(1600) {
            write_frame(&mut self.input, &Frame::Pcm(chunk.to_vec()))?;
        }
        self.call(
            "transcription.end",
            json!({"requestId": "dictation"}),
            &mut |_, _| {},
        )
    }
}

impl Drop for PluginProcess {
    fn drop(&mut self) {
        let _ = write_frame(
            &mut self.input,
            &Frame::Json(json!({"jsonrpc": "2.0", "id": 0, "method": "shutdown"})),
        );
        // Give the plugin a moment to exit on its own, then make sure it is gone.
        for _ in 0..20 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_plugin(root: &Path, folder: &str, version: &str, executable: &str) {
        let dir = root.join(folder);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = json!({
            "id": "com.typewhisper.example", "name": "Example", "version": version, "protocolVersion": 1,
            "minHostVersion": "0.1.0", "executable": executable, "categories": ["transcription"],
            "locality": "local", "architectures": ["x64", "arm64"]
        });
        std::fs::write(dir.join("manifest.json"), manifest.to_string()).unwrap();
    }

    fn temp_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("tw-plugin-host-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn highest_version_wins_and_system_wins_ties() {
        let root = temp_root("versions");
        let (user, system) = (root.join("user"), root.join("system"));
        write_plugin(&user, "a", "1.10.0", "bin/example");
        write_plugin(&system, "a", "1.9.3", "bin/example");
        let found = discover(&[(user.clone(), false), (system.clone(), true)]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].manifest.version, "1.10.0");

        write_plugin(&system, "a", "1.10.0", "bin/example");
        let found = discover(&[(user, false), (system, true)]);
        assert!(found[0].is_system);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_executables_outside_the_plugin_folder() {
        let root = temp_root("paths");
        write_plugin(&root, "a", "1.0.0", "../../bin/sh");
        write_plugin(&root, "b", "1.0.0", "/bin/sh");
        assert!(discover(&[(root.clone(), false)]).is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compares_versions_numerically() {
        assert_eq!(compare_versions("1.10.0", "1.9.9"), Ordering::Greater);
        assert_eq!(compare_versions("1.2", "1.2.0"), Ordering::Equal);
    }
}
