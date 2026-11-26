//! Finds installed plugins and runs each as its own process.
//!
//! Plugins live in one folder each, with `manifest.json` at the root. They come
//! from three places, searched in this order:
//!
//! 1. folders in `$TYPEWHISPER_PLUGIN_PATH` (colon-separated, for development),
//! 2. the user's plugin folder, filled by the in-app catalog,
//! 3. system folders: Flatpak extensions, each mounted at `/app/plugins/<Name>`
//!    with the plugin folder inside, and distribution packages under
//!    `/usr/lib/typewhisper/plugins`.
//!
//! For the same plugin ID the highest version wins; on a tie a system copy wins.

use std::cmp::Ordering;
use std::io::{self, BufReader, BufWriter};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tw_plugin_protocol::{
    CAPABILITY_SETTINGS, Frame, Manifest, PROTOCOL_VERSION, PluginSettings, SETTINGS_UPDATE,
    read_frame, write_frame,
};

/// Mount point of the `com.typewhisper.TypeWhisper.Plugin` extension point.
/// flatpak-builder writes its own `manifest.json` at the root of every
/// extension, so each extension holds its plugin folders one level down.
pub const FLATPAK_EXTENSIONS_DIR: &str = "/app/plugins";

pub const SYSTEM_PLUGIN_DIRS: &[&str] = &[
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
    dirs.extend(
        subfolders(Path::new(FLATPAK_EXTENSIONS_DIR))
            .into_iter()
            .map(|d| (d, true)),
    );
    dirs.extend(SYSTEM_PLUGIN_DIRS.iter().map(|d| (PathBuf::from(d), true)));
    dirs
}

/// The folders directly inside `dir`, sorted, or none if it does not exist.
fn subfolders(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    found.sort();
    found
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
    /// The plugin answered with a JSON-RPC error. `kind` is one of the names in
    /// [`tw_plugin_protocol::error_kind`] when the plugin sent one.
    Plugin {
        code: i64,
        message: String,
        kind: Option<String>,
    },
    /// The plugin closed its output or sent something that is not a response.
    Protocol(String),
    /// The plugin did not answer in time, so the host stopped it. Names the
    /// request, or `transcription` for [`PluginProcess::transcribe`].
    TimedOut(String),
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PluginError::Io(e) => write!(f, "plugin process: {e}"),
            PluginError::Plugin { message, .. } => write!(f, "{message}"),
            PluginError::Protocol(m) => write!(f, "plugin protocol: {m}"),
            PluginError::TimedOut(m) => write!(f, "plugin timed out during {m} and was stopped"),
        }
    }
}

impl From<io::Error> for PluginError {
    fn from(e: io::Error) -> Self {
        PluginError::Io(e)
    }
}

/// How long a plugin may take before the host stops it. A plugin that hangs
/// would otherwise keep its output open and block the caller for good.
#[derive(Debug, Clone, Copy)]
struct Limits {
    /// `initialize`, `transcription.begin` and any request not listed below.
    quick: Duration,
    /// `transcription.loadModel`.
    load_model: Duration,
    /// The longest silence during `models.download`; every notification restarts it.
    download_silence: Duration,
    /// A whole transcription may take this plus `transcribe_per_audio` times the
    /// length of its audio.
    transcribe_base: Duration,
    transcribe_per_audio: u32,
}

impl Limits {
    const DEFAULT: Limits = Limits {
        // Parakeet's `initialize` only checks which model files exist; 30 s leaves room
        // for starting the process from a cold disk on a busy machine.
        quick: Duration::from_secs(30),
        // Parakeet loads its 670 MB model in 1 to 2 s; two minutes also covers a slow
        // disk and a slow CPU.
        load_model: Duration::from_secs(120),
        // Parakeet reports every 1 % (about 5 MB) and stays silent while it unpacks, so
        // ten minutes are enough for a line of about 8 KB/s and a slow unpack.
        download_silence: Duration::from_secs(600),
        // Parakeet decodes 20 to 30 times faster than real time; real time plus 30 s
        // also covers a CPU many times slower and a cold first call.
        transcribe_base: Duration::from_secs(30),
        transcribe_per_audio: 1,
    };

    fn of(&self, method: &str) -> Limit {
        match method {
            "transcription.loadModel" => Limit::Total(self.load_model),
            "models.download" => Limit::Silence(self.download_silence),
            _ => Limit::Total(self.quick),
        }
    }

    /// Takes the transcription time a plugin asks for in `initialize`:
    ///
    /// ```json
    /// "transcriptionTimeout": {"baseSeconds": 180, "audioFactor": 3}
    /// ```
    ///
    /// Whisper's large models need many seconds per 30-second window on a CPU.
    /// The limits only grow, and only up to a cap, so that no plugin can switch
    /// the watchdog off.
    fn with_plugin_request(self, info: &Value) -> Limits {
        let requested = &info["transcriptionTimeout"];
        let base = requested["baseSeconds"]
            .as_u64()
            .map_or(self.transcribe_base, Duration::from_secs);
        let per_audio = requested["audioFactor"]
            .as_u64()
            .map_or(self.transcribe_per_audio, |f| f.min(1_000) as u32);
        Limits {
            transcribe_base: base.clamp(self.transcribe_base, MOST_TRANSCRIBE_BASE),
            transcribe_per_audio: per_audio
                .clamp(self.transcribe_per_audio, MOST_TRANSCRIBE_PER_AUDIO),
            ..self
        }
    }

    /// The time for `transcription.begin`, the audio and `transcription.end` together.
    fn transcription(&self, samples: usize) -> Duration {
        let audio = Duration::from_millis(samples as u64 * 1000 / SAMPLE_RATE);
        self.transcribe_base + audio * self.transcribe_per_audio
    }
}

/// Sample rate of the PCM frames the protocol carries.
const SAMPLE_RATE: u64 = 16_000;

/// The longest transcription limits a plugin may ask for.
const MOST_TRANSCRIBE_BASE: Duration = Duration::from_secs(600);
const MOST_TRANSCRIBE_PER_AUDIO: u32 = 10;

/// How long the plugin has before the host gives up on it.
#[derive(Debug, Clone, Copy)]
enum Limit {
    /// Until the response, however many notifications arrive.
    Total(Duration),
    /// Without any message from the plugin, so a long download stays alive while
    /// it reports progress.
    Silence(Duration),
}

impl Limit {
    fn duration(self) -> Duration {
        match self {
            Limit::Total(d) | Limit::Silence(d) => d,
        }
    }
}

/// A running plugin. Calls block, so use it from a worker thread.
pub struct PluginProcess {
    child: Arc<Mutex<Child>>,
    input: BufWriter<ChildStdin>,
    output: BufReader<ChildStdout>,
    next_id: u64,
    limits: Limits,
    watchdog: Watchdog,
    /// The `capabilities` the plugin returned from `initialize`.
    capabilities: Vec<String>,
}

impl PluginProcess {
    /// Starts the plugin and performs `initialize`. Returns the process and the
    /// plugin's answer (capabilities, models).
    pub fn start(
        plugin: &InstalledPlugin,
        data_dir: &Path,
        host_version: &str,
    ) -> Result<(Self, Value), PluginError> {
        Self::start_with(plugin, data_dir, host_version, Limits::DEFAULT)
    }

    fn start_with(
        plugin: &InstalledPlugin,
        data_dir: &Path,
        host_version: &str,
        limits: Limits,
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
        let child = Arc::new(Mutex::new(child));
        let watchdog = Watchdog::start(Arc::clone(&child))?;
        let mut process = Self {
            child,
            input,
            output,
            next_id: 0,
            limits,
            watchdog,
            capabilities: Vec::new(),
        };
        let info = process.call(
            "initialize",
            json!({"protocolVersion": PROTOCOL_VERSION, "hostVersion": host_version, "dataDir": data_dir}),
            &mut |_, _| {},
        )?;
        process.limits = limits.with_plugin_request(&info);
        process.capabilities = info["capabilities"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        Ok((process, info))
    }

    /// True when the plugin takes [`PluginSettings`].
    pub fn takes_settings(&self) -> bool {
        self.capabilities.iter().any(|c| c == CAPABILITY_SETTINGS)
    }

    /// Gives the plugin its settings, replacing the ones it had. Returns the
    /// plugin's answer, or `Null` without asking a plugin that takes no settings.
    pub fn configure(&mut self, settings: &PluginSettings) -> Result<Value, PluginError> {
        if !self.takes_settings() {
            return Ok(Value::Null);
        }
        let params = serde_json::to_value(settings).map_err(io::Error::from)?;
        self.call(SETTINGS_UPDATE, params, &mut |_, _| {})
    }

    /// Sends a request and waits for its response. Notifications that arrive in
    /// between (for example download progress) go to `on_notification`.
    ///
    /// A plugin that does not answer in time is stopped, and the call returns
    /// [`PluginError::TimedOut`]. The process cannot be used after that.
    pub fn call(
        &mut self,
        method: &str,
        params: Value,
        on_notification: &mut dyn FnMut(&str, &Value),
    ) -> Result<Value, PluginError> {
        let limit = self.limits.of(method);
        self.within(method, limit, |process| {
            process.request(method, params, limit, on_notification)
        })
    }

    /// Transcribes 16 kHz mono audio with the loaded model.
    pub fn transcribe(
        &mut self,
        samples: &[f32],
        language: Option<&str>,
    ) -> Result<Value, PluginError> {
        self.transcribe_with_prompt(samples, language, None)
    }

    /// Transcribes like [`Self::transcribe`] and passes `prompt`, words that
    /// help the model spell names and terms, to a plugin that uses one.
    pub fn transcribe_with_prompt(
        &mut self,
        samples: &[f32],
        language: Option<&str>,
        prompt: Option<&str>,
    ) -> Result<Value, PluginError> {
        let limit = Limit::Total(self.limits.transcription(samples.len()));
        let mut begin = json!({"requestId": "dictation", "language": language});
        if let Some(prompt) = prompt {
            begin["prompt"] = json!(prompt);
        }
        self.within("transcription", limit, |process| {
            process.request("transcription.begin", begin, limit, &mut |_, _| {})?;
            for chunk in samples.chunks(1600) {
                write_frame(&mut process.input, &Frame::Pcm(chunk.to_vec()))?;
            }
            process.request(
                "transcription.end",
                json!({"requestId": "dictation"}),
                limit,
                &mut |_, _| {},
            )
        })
    }

    /// Runs `work` under the watchdog. If the limit runs out, the watchdog kills
    /// the plugin, which ends any read or write that `work` is blocked in.
    fn within<T>(
        &mut self,
        what: &str,
        limit: Limit,
        work: impl FnOnce(&mut Self) -> Result<T, PluginError>,
    ) -> Result<T, PluginError> {
        self.watchdog.arm(limit.duration());
        let result = work(self);
        self.watchdog.disarm();
        if self.watchdog.fired() {
            return Err(PluginError::TimedOut(what.to_owned()));
        }
        result
    }

    fn request(
        &mut self,
        method: &str,
        params: Value,
        limit: Limit,
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
                                kind: error["data"]["kind"].as_str().map(str::to_owned),
                            });
                        }
                        return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                    }
                    if let Some(name) = message.get("method").and_then(Value::as_str) {
                        if let Limit::Silence(silence) = limit {
                            self.watchdog.arm(silence);
                        }
                        on_notification(name, message.get("params").unwrap_or(&Value::Null));
                    }
                }
            }
        }
    }
}

/// How long the plugin gets to exit after `shutdown`.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

impl Drop for PluginProcess {
    fn drop(&mut self) {
        // A plugin that no longer reads its input would block this write; the
        // watchdog ends it.
        self.watchdog.arm(SHUTDOWN_GRACE);
        let _ = write_frame(
            &mut self.input,
            &Frame::Json(json!({"jsonrpc": "2.0", "id": 0, "method": "shutdown"})),
        );
        // Give the plugin a moment to exit on its own, then make sure it is gone.
        let mut child = lock(&self.child);
        for _ in 0..20 {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(SHUTDOWN_GRACE / 20);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// One thread per plugin process that kills the plugin when a deadline passes.
/// Calls move the deadline; the thread sleeps until it is due.
struct Watchdog {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<WatchState>,
    changed: Condvar,
}

#[derive(Default)]
struct WatchState {
    deadline: Option<Instant>,
    /// The watchdog killed the plugin. It stays set, as the process is gone.
    fired: bool,
    stop: bool,
}

impl Watchdog {
    fn start(child: Arc<Mutex<Child>>) -> io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let thread = std::thread::Builder::new()
            .name("plugin-watchdog".into())
            .spawn({
                let shared = Arc::clone(&shared);
                move || watch(&shared, &child)
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Kills the plugin unless `disarm` or another `arm` comes within `limit`.
    fn arm(&self, limit: Duration) {
        lock(&self.shared.state).deadline = Some(Instant::now() + limit);
        self.shared.changed.notify_one();
    }

    fn disarm(&self) {
        lock(&self.shared.state).deadline = None;
    }

    fn fired(&self) -> bool {
        lock(&self.shared.state).fired
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        lock(&self.shared.state).stop = true;
        self.shared.changed.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn watch(shared: &Shared, child: &Mutex<Child>) {
    let mut state = lock(&shared.state);
    while !state.stop {
        let Some(deadline) = state.deadline else {
            state = shared
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
            continue;
        };
        let now = Instant::now();
        if now < deadline {
            state = shared
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            continue;
        }
        state.deadline = None;
        state.fired = true;
        drop(state);
        // The pipes close with the process, so the blocked read or write returns.
        let _ = lock(child).kill();
        state = lock(&shared.state);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
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
    fn finds_plugins_inside_flatpak_extensions() {
        let root = temp_root("extensions");
        let extension = root.join("Parakeet");
        write_plugin(
            &extension,
            "com.typewhisper.example",
            "1.0.0",
            "bin/example",
        );
        // flatpak-builder's own manifest at the extension root is no plugin.
        std::fs::write(extension.join("manifest.json"), r#"{"id": "x"}"#).unwrap();
        std::fs::write(root.join("stray-file"), "").unwrap();

        let roots = subfolders(&root);
        assert_eq!(roots, std::slice::from_ref(&extension));
        let found = discover(&[(extension, true)]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].manifest.id, "com.typewhisper.example");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compares_versions_numerically() {
        assert_eq!(compare_versions("1.10.0", "1.9.9"), Ordering::Greater);
        assert_eq!(compare_versions("1.2", "1.2.0"), Ordering::Equal);
    }

    /// Short limits for the fake plugins. `quick` still leaves them time to start
    /// on a busy machine.
    const FAST: Limits = Limits {
        quick: Duration::from_secs(2),
        load_model: Duration::from_millis(300),
        download_silence: Duration::from_secs(1),
        transcribe_base: Duration::from_millis(300),
        transcribe_per_audio: 0,
    };

    /// Longer than any test waits, so a plugin still sleeping proves that the
    /// watchdog ended it.
    const HANG: &str = "exec sleep 30";

    fn frame(message: Value) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &Frame::Json(message)).unwrap();
        bytes
    }

    fn response(id: u64, result: Value) -> Vec<u8> {
        frame(json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }

    /// A plugin made of a shell script. It runs in its own folder, where it can
    /// `cat` the given frames to answer requests.
    fn fake_plugin(root: &Path, script: &str, frames: &[(&str, Vec<u8>)]) -> InstalledPlugin {
        use std::os::unix::fs::PermissionsExt;
        write_plugin(root, "fake", "1.0.0", "plugin");
        let dir = root.join("fake");
        for (name, bytes) in frames {
            std::fs::write(dir.join(name), bytes).unwrap();
        }
        let executable = dir.join("plugin");
        std::fs::write(&executable, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        discover(&[(root.to_path_buf(), false)]).remove(0)
    }

    /// Starts a fake plugin. While one test writes its script, another may be
    /// starting a process that briefly inherits the open file, and exec then
    /// fails with "Text file busy". Trying again a moment later works.
    fn start_fake(
        plugin: &InstalledPlugin,
        root: &Path,
        limits: Limits,
    ) -> Result<(PluginProcess, Value), PluginError> {
        for _ in 0..50 {
            match PluginProcess::start_with(plugin, &root.join("data"), "0.1.0", limits) {
                Err(PluginError::Io(e)) if e.kind() == io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                started => return started,
            }
        }
        panic!("the fake plugin stayed busy");
    }

    fn killed(process: &PluginProcess) -> bool {
        use std::os::unix::process::ExitStatusExt;
        lock(&process.child).wait().unwrap().signal() == Some(9)
    }

    #[test]
    fn leaves_an_answering_plugin_alone() {
        let root = temp_root("answering");
        let plugin = fake_plugin(
            &root,
            // The download takes longer than its silence limit but reports progress
            // in between.
            "cat initialize.frame\n\
             for i in 1 2 3 4 5 6; do sleep 0.3; cat progress.frame; done\n\
             cat download.frame begin.frame end.frame\n\
             exec cat > /dev/null",
            &[
                ("initialize.frame", response(1, json!({"models": []}))),
                (
                    "progress.frame",
                    frame(json!({"jsonrpc": "2.0", "method": "models.progress"})),
                ),
                ("download.frame", response(2, json!({"installed": true}))),
                ("begin.frame", response(3, json!({}))),
                ("end.frame", response(4, json!({"text": "hello"}))),
            ],
        );
        let (mut process, _) = start_fake(&plugin, &root, FAST).unwrap();

        let mut progress = 0;
        let started = Instant::now();
        let downloaded = process.call("models.download", json!({}), &mut |method, _| {
            assert_eq!(method, "models.progress");
            progress += 1;
        });
        assert_eq!(downloaded.unwrap(), json!({"installed": true}));
        assert_eq!(progress, 6);
        assert!(started.elapsed() > FAST.download_silence);

        let transcribed = process.transcribe(&[0.0; 16_000], None).unwrap();
        assert_eq!(transcribed["text"], "hello");
        assert!(lock(&process.child).try_wait().unwrap().is_none());
        drop(process);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stops_a_plugin_that_does_not_answer() {
        let root = temp_root("silent");
        let plugin = fake_plugin(
            &root,
            &format!("cat initialize.frame\n{HANG}"),
            &[("initialize.frame", response(1, json!({})))],
        );
        let (mut process, _) = start_fake(&plugin, &root, FAST).unwrap();

        let started = Instant::now();
        let loaded = process.call("transcription.loadModel", json!({}), &mut |_, _| {});
        assert!(matches!(loaded, Err(PluginError::TimedOut(m)) if m == "transcription.loadModel"));
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(killed(&process));
        drop(process);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stops_a_plugin_that_hangs_on_start() {
        let root = temp_root("hanging-start");
        let plugin = fake_plugin(&root, &format!("echo $$ > pid\n{HANG}"), &[]);
        let limits = Limits {
            quick: Duration::from_millis(500),
            ..FAST
        };

        let started = Instant::now();
        let result = start_fake(&plugin, &root, limits);
        assert!(matches!(result, Err(PluginError::TimedOut(m)) if m == "initialize"));
        assert!(started.elapsed() < Duration::from_secs(10));
        let pid = std::fs::read_to_string(plugin.dir.join("pid")).unwrap();
        assert!(!Path::new("/proc").join(pid.trim()).exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stops_a_plugin_that_stops_reading_audio() {
        let root = temp_root("deaf");
        let plugin = fake_plugin(
            &root,
            &format!("cat initialize.frame begin.frame\n{HANG}"),
            &[
                ("initialize.frame", response(1, json!({}))),
                ("begin.frame", response(2, json!({}))),
            ],
        );
        let (mut process, _) = start_fake(&plugin, &root, FAST).unwrap();

        // Five seconds of audio overfill the pipe, so the host blocks while writing it.
        let started = Instant::now();
        let transcribed = process.transcribe(&[0.0; 5 * 16_000], None);
        assert!(matches!(transcribed, Err(PluginError::TimedOut(m)) if m == "transcription"));
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(killed(&process));
        drop(process);
        let _ = std::fs::remove_dir_all(root);
    }

    /// The frames the host wrote to a fake plugin that saved its input to `input`.
    fn requests(plugin: &InstalledPlugin) -> Vec<Value> {
        let bytes = std::fs::read(plugin.dir.join("input")).unwrap();
        let mut reader = bytes.as_slice();
        let mut found = Vec::new();
        while let Ok(Some(frame)) = read_frame(&mut reader) {
            if let Frame::Json(message) = frame {
                found.push(message);
            }
        }
        found
    }

    #[test]
    fn gives_settings_only_to_a_plugin_that_takes_them() {
        let root = temp_root("settings");
        let plugin = fake_plugin(
            &root,
            "cat initialize.frame settings.frame failure.frame\nexec cat > input",
            &[
                (
                    "initialize.frame",
                    response(1, json!({"capabilities": ["transcription", "settings"]})),
                ),
                ("settings.frame", response(2, json!({"configured": true}))),
                (
                    "failure.frame",
                    frame(json!({"jsonrpc": "2.0", "id": 3, "error": {
                        "code": -32020, "message": "Invalid API key",
                        "data": {"kind": "authentication"}
                    }})),
                ),
            ],
        );
        let (mut process, _) = start_fake(&plugin, &root, FAST).unwrap();
        assert!(process.takes_settings());
        let settings: PluginSettings = serde_json::from_value(json!({
            "values": {"selectedModel": "whisper-large-v3"}, "secrets": {"api-key": "gsk_test"}
        }))
        .unwrap();
        assert_eq!(
            process.configure(&settings).unwrap(),
            json!({"configured": true})
        );
        let failed = process.transcribe_with_prompt(&[0.0; 1600], Some("de"), Some("TypeWhisper"));
        assert!(matches!(
            failed,
            Err(PluginError::Plugin { code: -32020, kind: Some(k), .. }) if k == "authentication"
        ));
        drop(process);

        let sent = requests(&plugin);
        assert_eq!(sent[1]["method"], SETTINGS_UPDATE);
        assert_eq!(sent[1]["params"]["secrets"]["api-key"], "gsk_test");
        assert_eq!(
            sent[1]["params"]["values"]["selectedModel"],
            "whisper-large-v3"
        );
        assert_eq!(sent[2]["method"], "transcription.begin");
        assert_eq!(sent[2]["params"]["language"], "de");
        assert_eq!(sent[2]["params"]["prompt"], "TypeWhisper");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn keeps_settings_from_a_plugin_without_the_capability() {
        let root = temp_root("no-settings");
        let plugin = fake_plugin(
            &root,
            "cat initialize.frame\nexec cat > input",
            &[(
                "initialize.frame",
                response(1, json!({"capabilities": ["transcription"]})),
            )],
        );
        let (mut process, _) = start_fake(&plugin, &root, FAST).unwrap();
        assert!(!process.takes_settings());
        let mut settings = PluginSettings::default();
        settings.secrets.insert("api-key".into(), "gsk_test".into());
        assert_eq!(process.configure(&settings).unwrap(), Value::Null);
        drop(process);

        let sent = requests(&plugin);
        assert_eq!(sent[0]["method"], "initialize");
        assert!(sent.iter().all(|m| m["method"] != SETTINGS_UPDATE));
        let bytes = std::fs::read(plugin.dir.join("input")).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("gsk_test"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn gives_transcription_time_for_its_audio() {
        let minute = 60 * SAMPLE_RATE as usize;
        assert_eq!(
            Limits::DEFAULT.transcription(minute),
            Duration::from_secs(90)
        );
    }

    #[test]
    fn gives_a_slow_plugin_the_time_it_asks_for_within_bounds() {
        let minute = 60 * SAMPLE_RATE as usize;
        let ask = |base: u64, factor: u64| {
            Limits::DEFAULT
                .with_plugin_request(
                    &json!({"transcriptionTimeout": {"baseSeconds": base, "audioFactor": factor}}),
                )
                .transcription(minute)
        };
        assert_eq!(ask(180, 3), Duration::from_secs(180 + 3 * 60));
        // Never shorter than the default, never longer than the cap.
        assert_eq!(ask(1, 0), Duration::from_secs(90));
        assert_eq!(ask(1_000_000, 1_000), Duration::from_secs(600 + 10 * 60));
        let unchanged = Limits::DEFAULT.with_plugin_request(&json!({"models": []}));
        assert_eq!(unchanged.transcription(minute), Duration::from_secs(90));
        assert_eq!(unchanged.load_model, Limits::DEFAULT.load_model);
    }
}
