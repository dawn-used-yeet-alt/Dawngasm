//! Runtime extensions: small executables that add capabilities without being
//! linked into the TUI.
//!
//! Rust has no stable ABI, so a dynamically loaded `.so`/`.dll` plugin would
//! have to be built by the same compiler and crate versions as the host.
//! Extensions are therefore ordinary processes speaking one JSON request and
//! one JSON response over stdin/stdout. A spawn costs a few milliseconds, which
//! is noise next to the network requests an extractor makes, and a crashing
//! extension cannot take the player down.
//!
//! # Discovery
//!
//! Executables named `dawngasm-plugin-*` are picked up from:
//!
//! - `<exe dir>/plugins/` (the layout the release archive uses)
//! - `<exe dir>/` (so `cargo run` finds the bundled helper)
//! - `<data dir>/plugins/`
//! - every directory in `DAWNGASM_PLUGIN_PATH`, split like `PATH`
//!
//! A plugin is described by running it with `--describe`. Only plugins that
//! answer with a manifest are used.
//!
//! # Protocol
//!
//! `--describe` prints:
//!
//! ```json
//! {"id":"clearance","name":"Cloudflare clearance","version":"0.4.0","capabilities":["clearance"]}
//! ```
//!
//! `run` reads one request from stdin and writes one response to stdout:
//!
//! ```json
//! {"protocol":1,"method":"clearance.capture","params":{}}
//! {"protocol":1,"ok":true,"result":{"cookie":"...","user_agent":"..."}}
//! {"protocol":1,"ok":false,"error":"..."}
//! ```
//!
//! The host closes stdin after writing the request, so a plugin can read to
//! EOF. Anything the plugin writes to stderr is only shown when it fails.

use std::env;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::storage::Storage;

/// The protocol version both sides speak.
pub const PROTOCOL_VERSION: u32 = 1;
/// Environment variable with extra plugin directories, split like `PATH`.
pub const PLUGIN_PATH_ENV: &str = "DAWNGASM_PLUGIN_PATH";
/// Capability: capture a Cloudflare clearance cookie and user agent.
pub const CAP_CLEARANCE: &str = "clearance";
/// Capability: resolve a post to a plain HTTP media URL.
pub const CAP_EXTRACT: &str = "extract";
/// Method for [`CAP_CLEARANCE`].
pub const METHOD_CLEARANCE_CAPTURE: &str = "clearance.capture";
/// Method for [`CAP_EXTRACT`].
pub const METHOD_EXTRACT_RESOLVE: &str = "extract.resolve";

const PREFIX: &str = "dawngasm-plugin-";
const DESCRIBE: &str = "--describe";
const RUN: &str = "run";

/// Metadata a plugin prints for `--describe`.
#[derive(Clone, Debug, Deserialize)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// A discovered plugin.
#[derive(Clone, Debug)]
pub struct Extension {
    pub manifest: Manifest,
    pub path: PathBuf,
}

/// A candidate that could not be described, kept so the UI can explain why.
#[derive(Clone, Debug)]
pub struct ExtensionFailure {
    pub path: PathBuf,
    pub error: String,
}

impl Extension {
    /// Runs one request and returns its result.
    ///
    /// # Errors
    ///
    /// Returns an error when the plugin cannot be started, exits unsuccessfully,
    /// writes invalid JSON, or answers with `ok: false`.
    pub fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let request = Request {
            protocol: PROTOCOL_VERSION,
            method,
            params,
        };
        let mut child = Command::new(&self.path)
            .arg(RUN)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("could not start {}: {error}", self.manifest.id))?;
        if let Some(mut stdin) = child.stdin.take() {
            serde_json::to_writer(&mut stdin, &request)
                .map_err(|error| format!("could not write to {}: {error}", self.manifest.id))?;
            // The newline keeps line-oriented plugins simple; dropping stdin
            // then closes the pipe so a read-to-EOF plugin also finishes.
            let _ = stdin.write_all(b"\n");
        }
        let output = child
            .wait_with_output()
            .map_err(|error| format!("{} failed: {error}", self.manifest.id))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = stderr.trim();
            return Err(if message.is_empty() {
                format!("{} exited with {}", self.manifest.id, output.status)
            } else {
                message.to_string()
            });
        }
        let response: Response = serde_json::from_slice(&output.stdout)
            .map_err(|error| format!("{} returned invalid JSON: {error}", self.manifest.id))?;
        if response.ok {
            response
                .result
                .ok_or_else(|| format!("{} returned no result", self.manifest.id))
        } else {
            Err(response
                .error
                .unwrap_or_else(|| format!("{} failed without an error message", self.manifest.id)))
        }
    }
}

#[derive(Serialize)]
struct Request<'a> {
    protocol: u32,
    method: &'a str,
    params: Value,
}

#[derive(Deserialize)]
struct Response {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<String>,
}

/// All usable plugins, discovered once on first use.
pub struct Extensions {
    plugins: Vec<Extension>,
    failures: Vec<ExtensionFailure>,
}

impl Extensions {
    /// The process-wide set, loaded lazily so a normal startup never spawns a
    /// plugin.
    #[must_use]
    pub fn global() -> &'static Self {
        static EXTENSIONS: OnceLock<Extensions> = OnceLock::new();
        EXTENSIONS.get_or_init(Self::load)
    }

    /// The first plugin advertising `capability`, in discovery order.
    #[must_use]
    pub fn find(&self, capability: &str) -> Option<&Extension> {
        self.plugins.iter().find(|plugin| {
            plugin
                .manifest
                .capabilities
                .iter()
                .any(|candidate| candidate == capability)
        })
    }

    /// The plugins found on this machine. Broken candidates are recorded in
    /// [`failures`](Self::failure) instead of being used.
    #[must_use]
    pub fn load() -> Self {
        let mut plugins: Vec<Extension> = Vec::new();
        let mut failures: Vec<ExtensionFailure> = Vec::new();
        for path in candidates() {
            if plugins.iter().any(|plugin| plugin.path == path)
                || failures.iter().any(|failure| failure.path == path)
            {
                continue;
            }
            match describe(&path) {
                Ok(manifest) => plugins.push(Extension { manifest, path }),
                Err(error) => failures.push(ExtensionFailure { path, error }),
            }
        }
        Self { plugins, failures }
    }

    /// The loaded manifests, for diagnostics.
    #[must_use]
    pub fn manifests(&self) -> Vec<&Manifest> {
        self.plugins.iter().map(|plugin| &plugin.manifest).collect()
    }

    /// The first failed candidate whose filename contains `needle`.
    #[must_use]
    pub fn failure(&self, needle: &str) -> Option<&ExtensionFailure> {
        self.failures.iter().find(|failure| {
            failure
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(needle))
        })
    }
}

fn candidates() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.join("plugins"));
            dirs.push(dir.to_path_buf());
        }
    }
    if let Ok(data) = Storage::default_dir() {
        dirs.push(data.join("plugins"));
    }
    if let Some(value) = env::var_os(PLUGIN_PATH_ENV) {
        dirs.extend(env::split_paths(&value));
    }
    dirs.into_iter()
        .filter_map(|dir| fs::read_dir(dir).ok())
        .flat_map(|entries| entries.filter_map(Result::ok))
        .map(|entry| entry.path())
        .filter(|path| is_plugin_file(path))
        .collect()
}

fn is_plugin_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if !name.starts_with(PREFIX) || !path.is_file() {
        return false;
    }
    #[cfg(windows)]
    {
        path.extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        path.metadata()
            .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(any(unix, windows)))]
    {
        true
    }
}

fn describe(path: &Path) -> Result<Manifest, String> {
    let output = Command::new(path)
        .arg(DESCRIBE)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("could not start {}: {error}", path.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr.trim();
        return Err(if message.is_empty() {
            format!("{} exited with {}", path.display(), output.status)
        } else {
            message.to_string()
        });
    }
    let manifest: Manifest = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("{} returned an invalid manifest: {error}", path.display()))?;
    if manifest.id.trim().is_empty() || manifest.capabilities.is_empty() {
        return Err(format!(
            "{} has no id or capabilities in its manifest",
            path.display()
        ));
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_manifest() {
        let manifest: Manifest = serde_json::from_str(
            r#"{"id":"x","name":"X","version":"1","capabilities":["extract"]}"#,
        )
        .unwrap();
        assert_eq!(manifest.id, "x");
        assert!(manifest
            .capabilities
            .iter()
            .any(|cap| cap.as_str() == CAP_EXTRACT));
    }

    #[test]
    fn only_conventionally_named_plugins_are_candidates() {
        assert!(!is_plugin_file(Path::new("/nonexistent/dawngasm-tui")));
        assert!(!is_plugin_file(Path::new("/nonexistent/plugin-clearance")));
    }

    #[test]
    fn parses_success_and_error_responses() {
        let ok: Response =
            serde_json::from_str(r#"{"ok":true,"result":{"url":"https://cdn/x"}}"#).unwrap();
        assert!(ok.ok);
        assert_eq!(ok.result.unwrap()["url"], "https://cdn/x");

        let failed: Response =
            serde_json::from_str(r#"{"ok":false,"error":"no clearance"}"#).unwrap();
        assert!(!failed.ok);
        assert_eq!(failed.error.as_deref(), Some("no clearance"));
    }

    #[cfg(unix)]
    #[test]
    fn calls_a_plugin_over_json() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = env::temp_dir().join(format!("dawngasm-plugin-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dawngasm-plugin-test");
        fs::write(
            &path,
            "#!/bin/sh\ncat >/dev/null\nprintf '{\"ok\":true,\"result\":{\"echo\":1}}'\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let plugin = Extension {
            manifest: Manifest {
                id: "test".into(),
                name: "Test".into(),
                version: String::new(),
                capabilities: vec![CAP_EXTRACT.into()],
            },
            path,
        };
        let result = plugin
            .call(
                METHOD_EXTRACT_RESOLVE,
                serde_json::json!({"url": "https://example.com"}),
            )
            .unwrap();
        assert_eq!(result["echo"], 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
