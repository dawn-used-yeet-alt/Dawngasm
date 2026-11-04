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
//! Executables named `dawngasm-plugin-*` are picked up from, in precedence
//! order:
//!
//! 1. every directory in `DAWNGASM_PLUGIN_PATH`, split like `PATH`
//! 2. `<exe dir>/plugins/` (the layout the release archive uses)
//! 3. `<exe dir>/` (so `cargo run` finds the bundled helper)
//! 4. `<data dir>/plugins/`
//!
//! Entries are sorted so "first one found" is deterministic, and the same file
//! reached through two directories is only loaded once.
//!
//! A plugin is described by running it with `--describe`. Only plugins that
//! answer with a valid manifest are used; broken candidates are kept in
//! [`Extensions::failures`] so the UI and `extensions list` can explain them.
//!
//! # Protocol
//!
//! Version 2 is the current protocol. Version 1 plugins keep working: the host
//! sends `"protocol": 1` to them and accepts the older string-only error.
//!
//! `--describe` prints a manifest:
//!
//! ```json
//! {
//!   "protocol": 2,
//!   "id": "clearance",
//!   "name": "Cloudflare clearance webview",
//!   "version": "0.5.0",
//!   "description": "Opens hotaudio in the system webview.",
//!   "capabilities": [
//!     {"name": "clearance", "methods": ["clearance.capture"], "timeout_ms": 330000}
//!   ],
//!   "commands": []
//! }
//! ```
//!
//! `capabilities` also accepts the version 1 form, an array of names:
//! `["clearance", "extract"]`.
//!
//! `run` reads one request from stdin and writes one response to stdout:
//!
//! ```json
//! {"protocol":2,"id":"1234-0","method":"extract.resolve","params":{}}
//! {"protocol":2,"id":"1234-0","ok":true,"result":{"url":"https://cdn/x"}}
//! {"protocol":2,"id":"1234-0","ok":false,"error":{"code":"unsupported","message":"not my host"}}
//! ```
//!
//! The host closes stdin after writing the request, so a plugin can read to
//! EOF. Anything the plugin writes to stderr is only shown when it fails.
//!
//! # Limits
//!
//! Every call has a timeout and an output cap. They can be tuned with
//! `DAWNGASM_PLUGIN_TIMEOUT_MS`, `DAWNGASM_PLUGIN_DESCRIBE_TIMEOUT_MS` and
//! `DAWNGASM_PLUGIN_MAX_OUTPUT`. A plugin that hangs or floods its pipes is
//! killed and reported as an error instead of freezing the app.

use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs;
use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, OnceLock, PoisonError, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::models::post::Post;

use super::storage::Storage;

/// The protocol version the host speaks.
pub const PROTOCOL_VERSION: u32 = 2;
/// The oldest protocol version still accepted.
pub const MIN_PROTOCOL_VERSION: u32 = 1;
/// Environment variable with extra plugin directories, split like `PATH`.
pub const PLUGIN_PATH_ENV: &str = "DAWNGASM_PLUGIN_PATH";

/// Capability: capture a Cloudflare clearance cookie and user agent.
pub const CAP_CLEARANCE: &str = "clearance";
/// Capability: resolve a post to a plain HTTP media URL.
pub const CAP_EXTRACT: &str = "extract";
/// Capability: provide feed pages and tag suggestions.
pub const CAP_FEED: &str = "feed";
/// Capability: drop posts from a fetched page.
pub const CAP_FILTER: &str = "post.filter";
/// Capability: attach badges and tags to fetched posts.
pub const CAP_ENRICH: &str = "post.enrich";
/// Capability: expose user-invokable commands.
pub const CAP_COMMAND: &str = "command";

/// Method for [`CAP_CLEARANCE`].
pub const METHOD_CLEARANCE_CAPTURE: &str = "clearance.capture";
/// Method for [`CAP_EXTRACT`].
pub const METHOD_EXTRACT_RESOLVE: &str = "extract.resolve";
/// Method for [`CAP_FEED`] that returns one page of posts.
pub const METHOD_FEED_FETCH: &str = "feed.fetch";
/// Method for [`CAP_FEED`] that returns tag suggestions.
pub const METHOD_FEED_TAGS: &str = "feed.tags";
/// Method for [`CAP_FILTER`].
pub const METHOD_FILTER_APPLY: &str = "post.filter";
/// Method for [`CAP_ENRICH`].
pub const METHOD_ENRICH_APPLY: &str = "post.enrich";
/// Method for [`CAP_COMMAND`].
pub const METHOD_COMMAND_INVOKE: &str = "command.invoke";

const PREFIX: &str = "dawngasm-plugin-";
const DESCRIBE: &str = "--describe";
const RUN: &str = "run";
const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// How long to wait for the reader threads after the child exits. A forked
/// descendant can inherit the pipes and keep them open forever, so this is
/// bounded rather than a plain `join()`.
const READER_GRACE: Duration = Duration::from_secs(1);
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_DESCRIBE_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_MAX_OUTPUT: usize = 1024 * 1024;

/// Metadata a plugin prints for `--describe`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Manifest {
    /// Protocol version the plugin speaks. Missing means version 1.
    #[serde(default = "default_protocol")]
    pub protocol: u32,
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, deserialize_with = "deserialize_capabilities")]
    pub capabilities: Vec<CapabilityDecl>,
    #[serde(default)]
    pub commands: Vec<CommandDecl>,
}

const fn default_protocol() -> u32 {
    MIN_PROTOCOL_VERSION
}

impl Manifest {
    /// The name to show in errors, falling back to the id.
    #[must_use]
    pub fn display_name(&self) -> &str {
        if self.name.is_empty() {
            &self.id
        } else {
            &self.name
        }
    }

    /// The declaration for `capability`, if this plugin advertises it.
    #[must_use]
    pub fn capability(&self, capability: &str) -> Option<&CapabilityDecl> {
        self.capabilities
            .iter()
            .find(|candidate| candidate.name == capability)
    }

    /// Whether this plugin advertises `capability`.
    #[must_use]
    pub fn supports(&self, capability: &str) -> bool {
        self.capability(capability).is_some()
    }

    /// The highest priority across all capabilities. Dispatch orders by the
    /// priority of the capability being used; this is a convenience for
    /// callers that want one ordering score for the whole plugin.
    #[must_use]
    pub fn priority(&self) -> i32 {
        self.capabilities
            .iter()
            .map(|capability| capability.priority)
            .max()
            .unwrap_or(0)
    }

    fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("manifest has no id".to_string());
        }
        if !(MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&self.protocol) {
            return Err(format!(
                "unsupported protocol {}; this host speaks {MIN_PROTOCOL_VERSION}..={PROTOCOL_VERSION}",
                self.protocol
            ));
        }
        if self.capabilities.is_empty() {
            return Err("manifest has no capabilities".to_string());
        }
        for capability in &self.capabilities {
            if capability.name.trim().is_empty() {
                return Err("a capability has no name".to_string());
            }
        }
        for command in &self.commands {
            if command.id.trim().is_empty() {
                return Err("a command has no id".to_string());
            }
        }
        Ok(())
    }
}

/// One capability a plugin advertises.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct CapabilityDecl {
    /// Capability name, e.g. `extract` or `post.enrich`.
    pub name: String,
    /// Methods this capability answers. Empty means the capability's default
    /// method set.
    #[serde(default)]
    pub methods: Vec<String>,
    /// Host patterns for `extract`: `example.com`, `.example.com` or
    /// `*.example.com`. Empty means the plugin handles any host.
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Higher runs first when several plugins advertise the same capability.
    #[serde(default)]
    pub priority: i32,
    /// For `extract`: try this plugin before the built-in extractor even when
    /// the host is already supported.
    #[serde(default)]
    pub override_builtin: bool,
    /// Per-call timeout in milliseconds. Overrides the capability default.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

impl CapabilityDecl {
    /// The methods this capability answers.
    #[must_use]
    pub fn methods(&self) -> Vec<&str> {
        if self.methods.is_empty() {
            default_methods(&self.name).to_vec()
        } else {
            self.methods.iter().map(String::as_str).collect()
        }
    }

    /// Whether this capability answers `method`.
    #[must_use]
    pub fn handles_method(&self, method: &str) -> bool {
        self.methods().contains(&method)
    }
}

/// One user-invokable command a plugin exposes.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CommandDecl {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// Optional suggested key. The host currently runs commands through the
    /// `:` palette, but records this so future versions can bind it.
    #[serde(default)]
    pub key: Option<String>,
}

impl CommandDecl {
    /// The title to show, falling back to the id.
    #[must_use]
    pub fn display_title(&self) -> &str {
        if self.title.is_empty() {
            &self.id
        } else {
            &self.title
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CapabilityWire {
    Name(String),
    Decl(CapabilityDecl),
}

fn deserialize_capabilities<'de, D>(deserializer: D) -> Result<Vec<CapabilityDecl>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<CapabilityWire>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .map(|wire| match wire {
            CapabilityWire::Name(name) => CapabilityDecl {
                name,
                ..CapabilityDecl::default()
            },
            CapabilityWire::Decl(decl) => decl,
        })
        .collect())
}

fn default_methods(capability: &str) -> &'static [&'static str] {
    match capability {
        CAP_CLEARANCE => &[METHOD_CLEARANCE_CAPTURE],
        CAP_EXTRACT => &[METHOD_EXTRACT_RESOLVE],
        CAP_FEED => &[METHOD_FEED_FETCH, METHOD_FEED_TAGS],
        CAP_FILTER => &[METHOD_FILTER_APPLY],
        CAP_ENRICH => &[METHOD_ENRICH_APPLY],
        CAP_COMMAND => &[METHOD_COMMAND_INVOKE],
        _ => &[],
    }
}

/// A discovered plugin.
#[derive(Clone, Debug)]
pub struct Extension {
    pub manifest: Manifest,
    pub path: PathBuf,
}

impl Extension {
    /// Whether this plugin advertises `capability`.
    #[must_use]
    pub fn supports(&self, capability: &str) -> bool {
        self.manifest.supports(capability)
    }

    /// The declaration for `capability`.
    #[must_use]
    pub fn capability(&self, capability: &str) -> Option<&CapabilityDecl> {
        self.manifest.capability(capability)
    }

    /// Whether this plugin asked to run before the built-in extractor.
    #[must_use]
    pub fn overrides_builtin(&self, capability: &str) -> bool {
        self.capability(capability)
            .is_some_and(|declaration| declaration.override_builtin)
    }

    /// Runs one request and returns its result.
    ///
    /// # Errors
    ///
    /// Returns an [`ExtensionError`] when the plugin cannot be started, exceeds
    /// its timeout or output cap, exits unsuccessfully, writes invalid JSON,
    /// answers with the wrong protocol or request id, or returns `ok: false`.
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)] // Protocol checks read best together.
    pub fn call(&self, method: &str, params: Value) -> Result<Value, ExtensionError> {
        let protocol = if self.manifest.protocol >= 2 {
            PROTOCOL_VERSION
        } else {
            1
        };
        let id = next_request_id();
        let request = Request {
            protocol,
            // Version 1 plugins predate request ids; sending one to a strict
            // deserializer would break a plugin that used to work.
            id: (protocol >= 2).then_some(id.as_str()),
            method,
            params,
        };
        let body = serde_json::to_vec(&request).map_err(|error| {
            ExtensionError::new(
                &self.manifest.id,
                "encode",
                format!("could not encode the request: {error}"),
            )
        })?;

        let mut command = Command::new(&self.path);
        command.arg(RUN);
        let captured = run_child(
            command,
            Some(body.as_slice()),
            self.timeout_for(method),
            max_output(),
        )
        .map_err(|error| ExtensionError::new(&self.manifest.id, "spawn", error))?;

        if captured.timed_out {
            return Err(ExtensionError::retryable(
                &self.manifest.id,
                "timeout",
                format!(
                    "{} did not answer `{method}` within {:?}",
                    self.manifest.display_name(),
                    self.timeout_for(method)
                ),
            ));
        }
        if captured.overflow {
            return Err(ExtensionError::new(
                &self.manifest.id,
                "overflow",
                format!(
                    "{} wrote more than {} bytes",
                    self.manifest.display_name(),
                    max_output()
                ),
            ));
        }
        if !captured.status.success() {
            let stderr = String::from_utf8_lossy(&captured.stderr);
            let message = stderr.trim();
            return Err(ExtensionError::new(
                &self.manifest.id,
                "exit",
                if message.is_empty() {
                    format!(
                        "{} exited with {}",
                        self.manifest.display_name(),
                        captured.status
                    )
                } else {
                    message.to_string()
                },
            ));
        }

        let response: Response = serde_json::from_slice(&captured.stdout).map_err(|error| {
            ExtensionError::new(
                &self.manifest.id,
                "protocol",
                format!(
                    "{} returned invalid JSON: {error}",
                    self.manifest.display_name()
                ),
            )
        })?;
        if let Some(protocol) = response.protocol {
            if !(MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&protocol) {
                return Err(ExtensionError::new(
                    &self.manifest.id,
                    "protocol",
                    format!(
                        "{} answered with unsupported protocol {protocol}",
                        self.manifest.display_name()
                    ),
                ));
            }
        }
        if protocol >= 2 && response.id.is_none() {
            return Err(ExtensionError::new(
                &self.manifest.id,
                "protocol",
                format!(
                    "{} did not echo the request id",
                    self.manifest.display_name()
                ),
            ));
        }
        if let Some(response_id) = response.id.as_deref() {
            if response_id != id {
                return Err(ExtensionError::new(
                    &self.manifest.id,
                    "protocol",
                    format!(
                        "{} answered request {response_id}, expected {id}",
                        self.manifest.display_name()
                    ),
                ));
            }
        }
        if response.ok {
            response.result.ok_or_else(|| {
                ExtensionError::new(
                    &self.manifest.id,
                    "protocol",
                    format!("{} returned no result", self.manifest.display_name()),
                )
            })
        } else {
            Err(response.error.map_or_else(
                || {
                    ExtensionError::new(
                        &self.manifest.id,
                        "plugin",
                        format!(
                            "{} failed without an error message",
                            self.manifest.display_name()
                        ),
                    )
                },
                |error| error.into_parts(&self.manifest.id),
            ))
        }
    }

    fn timeout_for(&self, method: &str) -> Duration {
        if let Some(capability) = self
            .manifest
            .capabilities
            .iter()
            .find(|capability| capability.handles_method(method))
        {
            if let Some(millis) = capability.timeout_ms {
                return Duration::from_millis(millis);
            }
            return default_timeout(&capability.name);
        }
        default_timeout("")
    }
}

/// A plugin call that failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionError {
    /// The plugin id.
    pub plugin: String,
    /// A machine-readable code such as `timeout`, `protocol` or `unsupported`.
    pub code: String,
    /// The user-facing message.
    pub message: String,
    /// Whether retrying the same call could succeed.
    pub retryable: bool,
}

impl ExtensionError {
    fn new(plugin: &str, code: &str, message: impl Into<String>) -> Self {
        Self {
            plugin: plugin.to_string(),
            code: code.to_string(),
            message: message.into(),
            retryable: false,
        }
    }

    fn retryable(plugin: &str, code: &str, message: impl Into<String>) -> Self {
        Self {
            retryable: true,
            ..Self::new(plugin, code, message)
        }
    }

    /// Whether the plugin explicitly declined the work, so the host should try
    /// the next plugin. `handled: false` in a successful result is the
    /// preferred signal; these error codes are the fallback.
    #[must_use]
    pub fn is_unhandled(&self) -> bool {
        matches!(
            self.code.as_str(),
            "unsupported" | "not_found" | "unhandled"
        )
    }
}

impl std::fmt::Display for ExtensionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.plugin, self.message)
    }
}

impl std::error::Error for ExtensionError {}

/// A candidate that could not be described, kept so the UI can explain why.
#[derive(Clone, Debug)]
pub struct ExtensionFailure {
    pub path: PathBuf,
    pub error: String,
}

/// A command exposed by an extension, with the plugin that owns it.
#[derive(Clone, Copy, Debug)]
pub struct CommandRef<'a> {
    pub plugin: &'a Extension,
    pub command: &'a CommandDecl,
}

/// One page returned by a `feed` extension.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FeedPage {
    #[serde(default)]
    pub posts: Vec<Post>,
    #[serde(default)]
    pub has_more: bool,
}

/// Badges and tags attached to a post by an `post.enrich` extension.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct PostAnnotations {
    #[serde(default)]
    pub badges: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// The result of a `post.filter` extension.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FilterResult {
    /// Post keys to keep. Missing means "keep everything".
    #[serde(default)]
    pub keep: Option<Vec<String>>,
}

/// The result of a `post.enrich` extension.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EnrichResult {
    #[serde(default)]
    pub annotations: BTreeMap<String, PostAnnotations>,
}

/// All usable plugins, discovered once on first use and reloadable.
pub struct Extensions {
    plugins: Vec<Extension>,
    failures: Vec<ExtensionFailure>,
}

static EXTENSIONS: OnceLock<RwLock<Arc<Extensions>>> = OnceLock::new();

impl Extensions {
    /// The process-wide set, loaded lazily on first use so a command-line
    /// query does not scan for plugins until it asks for them.
    #[must_use]
    pub fn global() -> Arc<Self> {
        let lock = EXTENSIONS.get_or_init(|| RwLock::new(Arc::new(Self::load())));
        Arc::clone(&lock.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Re-scans the plugin directories and replaces the process-wide set.
    /// Calls already holding the old [`Arc`] keep using it.
    ///
    /// This is public API for embedders; the TUI itself loads once per process
    /// and does not call it yet.
    #[must_use]
    pub fn reload() -> Arc<Self> {
        let lock = EXTENSIONS.get_or_init(|| RwLock::new(Arc::new(Self::load())));
        let loaded = Arc::new(Self::load());
        *lock.write().unwrap_or_else(PoisonError::into_inner) = Arc::clone(&loaded);
        loaded
    }

    /// The plugins found on this machine. Broken candidates are recorded in
    /// [`failures`](Self::failures) instead of being used.
    #[must_use]
    pub fn load() -> Self {
        let mut plugins = Vec::new();
        let mut failures = Vec::new();
        for (path, result) in describe_all(candidates()) {
            match result {
                Ok(manifest) => plugins.push(Extension { manifest, path }),
                Err(error) => failures.push(ExtensionFailure { path, error }),
            }
        }
        Self { plugins, failures }
    }

    /// The first plugin advertising `capability`, highest priority first.
    #[must_use]
    pub fn find(&self, capability: &str) -> Option<&Extension> {
        self.find_all(capability).into_iter().next()
    }

    /// Every plugin advertising `capability`, highest priority first.
    #[must_use]
    pub fn find_all(&self, capability: &str) -> Vec<&Extension> {
        let mut found: Vec<&Extension> = self
            .plugins
            .iter()
            .filter(|plugin| plugin.supports(capability))
            .collect();
        sort_by_capability_priority(&mut found, capability);
        found
    }

    /// The first plugin advertising `capability` and answering `method`.
    #[must_use]
    pub fn find_for_method(&self, capability: &str, method: &str) -> Option<&Extension> {
        self.find_all_for_method(capability, method)
            .into_iter()
            .next()
    }

    /// Every plugin advertising `capability` and answering `method`, highest
    /// priority first.
    ///
    /// A capability's `methods` list narrows it: a plugin that only declares
    /// `feed.tags` is not offered `feed.fetch`, even though it advertises the
    /// `feed` capability.
    #[must_use]
    pub fn find_all_for_method(&self, capability: &str, method: &str) -> Vec<&Extension> {
        let mut found: Vec<&Extension> = self
            .plugins
            .iter()
            .filter(|plugin| {
                plugin
                    .capability(capability)
                    .is_some_and(|declaration| declaration.handles_method(method))
            })
            .collect();
        sort_by_capability_priority(&mut found, capability);
        found
    }

    /// The `extract` plugins that handle any of `urls`, most specific host
    /// match first, then highest priority.
    #[must_use]
    pub fn extractors_for_urls(&self, urls: &[&str]) -> Vec<&Extension> {
        let hosts: Vec<String> = urls.iter().filter_map(|url| host_of(url)).collect();
        let mut found: Vec<(&Extension, (u8, usize), i32)> = self
            .plugins
            .iter()
            .filter_map(|plugin| {
                let capability = plugin.capability(CAP_EXTRACT)?;
                if !capability.handles_method(METHOD_EXTRACT_RESOLVE) {
                    return None;
                }
                let specificity = if capability.hosts.is_empty() {
                    (0, 0)
                } else {
                    hosts
                        .iter()
                        .map(|host| host_specificity(&capability.hosts, host))
                        .max()
                        .unwrap_or((0, 0))
                };
                if !capability.hosts.is_empty() && specificity.0 == 0 {
                    return None;
                }
                Some((plugin, specificity, capability.priority))
            })
            .collect();
        found.sort_by(
            |(_, a_specificity, a_priority), (_, b_specificity, b_priority)| {
                b_specificity
                    .cmp(a_specificity)
                    .then_with(|| b_priority.cmp(a_priority))
            },
        );
        found.into_iter().map(|(plugin, _, _)| plugin).collect()
    }

    /// Every command exposed by a `command` plugin.
    #[must_use]
    pub fn commands(&self) -> Vec<CommandRef<'_>> {
        let mut commands = Vec::new();
        for plugin in &self.plugins {
            let Some(capability) = plugin.capability(CAP_COMMAND) else {
                continue;
            };
            if !capability.handles_method(METHOD_COMMAND_INVOKE) {
                continue;
            }
            for command in &plugin.manifest.commands {
                commands.push(CommandRef { plugin, command });
            }
        }
        commands.sort_by(|a, b| a.command.id.cmp(&b.command.id));
        commands
    }

    /// The plugin with `id`, if loaded.
    #[must_use]
    pub fn find_by_id(&self, id: &str) -> Option<&Extension> {
        self.plugins.iter().find(|plugin| plugin.manifest.id == id)
    }

    /// A plugin by id, or failing that the first one advertising `reference` as
    /// a capability. Used by the `extensions call` command.
    #[must_use]
    pub fn find_by_ref(&self, reference: &str) -> Option<&Extension> {
        self.find_by_id(reference).or_else(|| self.find(reference))
    }

    /// The loaded plugins, for diagnostics.
    #[must_use]
    pub fn plugins(&self) -> &[Extension] {
        &self.plugins
    }

    /// Every candidate that could not be described.
    #[must_use]
    pub fn failures(&self) -> &[ExtensionFailure] {
        &self.failures
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

    /// The directories scanned for plugins, in precedence order.
    #[must_use]
    pub fn search_dirs() -> Vec<PathBuf> {
        candidate_dirs()
    }
}

/// Whether a successful `extract` result explicitly declined the post.
#[must_use]
pub fn is_unhandled(value: &Value) -> bool {
    value.get("handled").and_then(Value::as_bool) == Some(false)
}

/// Extracts the lowercased host from a URL or authority.
#[must_use]
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?;
    let host = match authority.strip_prefix('[') {
        Some(stripped) => stripped.split(']').next()?,
        None => authority.split(':').next()?,
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

/// Extracts the URLs mentioned in free text. Enough for a post body without
/// pulling in a full URL parser.
#[must_use]
pub fn urls_in_text(text: &str) -> Vec<&str> {
    let mut urls = Vec::new();
    let mut rest = text;
    while let Some(index) = rest.find("http") {
        let tail = &rest[index..];
        if tail.starts_with("http://") || tail.starts_with("https://") {
            let end = tail
                .find(|character: char| {
                    character.is_whitespace()
                        || matches!(character, ')' | ']' | '"' | '\'' | '>' | '<')
                })
                .unwrap_or(tail.len());
            urls.push(&tail[..end]);
            rest = &tail[end..];
        } else {
            rest = &tail[4..];
        }
    }
    urls
}

/// Whether `host` matches any of `patterns`.
///
/// Patterns are exact domains (`example.com`), or `example.com` prefixed with
/// `.` or `*.` to include subdomains. Matching is case-insensitive.
#[must_use]
pub fn host_matches(patterns: &[String], host: &str) -> bool {
    patterns.iter().any(|pattern| {
        let pattern = pattern.trim().trim_end_matches('.').to_ascii_lowercase();
        if pattern.is_empty() {
            return false;
        }
        pattern
            .strip_prefix("*.")
            .or_else(|| pattern.strip_prefix('.'))
            .map_or_else(
                || host == pattern,
                |suffix| host == suffix || host.ends_with(&format!(".{suffix}")),
            )
    })
}

fn capability_priority(plugin: &Extension, capability: &str) -> i32 {
    plugin
        .capability(capability)
        .map_or(0, |declaration| declaration.priority)
}

/// Sorts highest priority first. Stable, so equal priorities keep discovery
/// order. The priority is read from `capability` rather than the whole
/// manifest, so one high-priority capability does not reorder the others.
fn sort_by_capability_priority(found: &mut [&Extension], capability: &str) {
    found.sort_by(|a, b| {
        capability_priority(b, capability).cmp(&capability_priority(a, capability))
    });
}

/// How specifically `patterns` match `host`.
///
/// The first component ranks an exact domain above a wildcard. The second is
/// the length of the matched suffix, so `*.cdn.example.com` outranks
/// `*.example.com` for `cdn.example.com`.
fn host_specificity(patterns: &[String], host: &str) -> (u8, usize) {
    let mut best = (0, 0);
    for pattern in patterns {
        if !host_matches(std::slice::from_ref(pattern), host) {
            continue;
        }
        let trimmed = pattern.trim().trim_end_matches('.');
        let score = trimmed
            .strip_prefix("*.")
            .or_else(|| trimmed.strip_prefix('.'))
            .map_or((2, trimmed.len()), |suffix| (1, suffix.len()));
        best = best.max(score);
    }
    best
}

#[derive(Serialize)]
struct Request<'a> {
    protocol: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<&'a str>,
    method: &'a str,
    params: Value,
}

#[derive(Deserialize)]
struct Response {
    #[serde(default)]
    protocol: Option<u32>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<ErrorWire>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ErrorWire {
    Message(String),
    Detail {
        #[serde(default)]
        code: Option<String>,
        #[serde(default)]
        message: String,
        #[serde(default)]
        retryable: Option<bool>,
    },
}

impl ErrorWire {
    fn into_parts(self, plugin: &str) -> ExtensionError {
        match self {
            Self::Message(message) => ExtensionError::new(plugin, "plugin", message),
            Self::Detail {
                code,
                message,
                retryable,
            } => ExtensionError {
                plugin: plugin.to_string(),
                code: code.unwrap_or_else(|| "plugin".to_string()),
                message: if message.is_empty() {
                    "the plugin failed without a message".to_string()
                } else {
                    message
                },
                retryable: retryable.unwrap_or(false),
            },
        }
    }
}

struct Captured {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timed_out: bool,
    overflow: bool,
}

fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(value) = env::var_os(PLUGIN_PATH_ENV) {
        dirs.extend(env::split_paths(&value));
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.join("plugins"));
            dirs.push(dir.to_path_buf());
        }
    }
    if let Ok(data) = Storage::default_dir() {
        dirs.push(data.join("plugins"));
    }
    dirs
}

fn candidates() -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for dir in candidate_dirs() {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            if !is_plugin_file(&path) {
                continue;
            }
            let key = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if seen.insert(key) {
                paths.push(path);
            }
        }
    }
    paths
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

/// Describes candidates with a small worker pool, preserving discovery order.
fn describe_all(paths: Vec<PathBuf>) -> Vec<(PathBuf, Result<Manifest, String>)> {
    const WORKERS: usize = 4;
    let workers = paths.len().min(WORKERS);
    if workers <= 1 {
        return paths
            .into_iter()
            .map(|path| {
                let result = describe(&path);
                (path, result)
            })
            .collect();
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut collected = Vec::with_capacity(paths.len());
    thread::scope(|scope| {
        let (sender, receiver) = mpsc::channel();
        for _ in 0..workers {
            let sender = sender.clone();
            let next = &next;
            let paths = &paths;
            scope.spawn(move || loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(path) = paths.get(index) else {
                    break;
                };
                if sender.send((index, describe(path))).is_err() {
                    break;
                }
            });
        }
        drop(sender);
        collected.extend(receiver);
    });
    collected.sort_by_key(|(index, _)| *index);
    collected
        .into_iter()
        .map(|(index, result)| (paths[index].clone(), result))
        .collect()
}

fn describe(path: &Path) -> Result<Manifest, String> {
    let mut command = Command::new(path);
    command.arg(DESCRIBE);
    let captured = run_child(command, None, describe_timeout(), max_output())?;
    if captured.timed_out {
        return Err(format!(
            "did not answer {DESCRIBE} within {:?}",
            describe_timeout()
        ));
    }
    if captured.overflow {
        return Err(format!(
            "wrote more than {} bytes to {DESCRIBE}",
            max_output()
        ));
    }
    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        let message = stderr.trim();
        return Err(if message.is_empty() {
            format!("exited with {}", captured.status)
        } else {
            message.to_string()
        });
    }
    let manifest: Manifest = serde_json::from_slice(&captured.stdout)
        .map_err(|error| format!("returned an invalid manifest: {error}"))?;
    manifest.validate()?;
    Ok(manifest)
}

fn run_child(
    mut command: Command,
    input: Option<&[u8]>,
    timeout: Duration,
    limit: usize,
) -> Result<Captured, String> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start the plugin: {error}"))?;

    if let Some(input) = input {
        if let Some(mut stdin) = child.stdin.take() {
            let input = input.to_vec();
            // Detached on purpose: a plugin may exit before reading, and the
            // host should not block on a pipe nobody is draining.
            let _ = thread::spawn(move || {
                let _ = stdin.write_all(&input);
                let _ = stdin.write_all(b"\n");
            });
        }
    }

    let overflow = Arc::new(AtomicBool::new(false));
    let stdout = child.stdout.take().map(|pipe| {
        let overflow = Arc::clone(&overflow);
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let _ = sender.send(read_limited(pipe, limit, &overflow));
        });
        receiver
    });
    let stderr = child.stderr.take().map(|pipe| {
        let overflow = Arc::clone(&overflow);
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let _ = sender.send(read_limited(pipe, limit, &overflow));
        });
        receiver
    });

    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if overflow.load(Ordering::Relaxed) {
                    let _ = child.kill();
                    break child
                        .wait()
                        .map_err(|error| format!("could not wait for the plugin: {error}"))?;
                }
                if started.elapsed() >= timeout {
                    timed_out = true;
                    let _ = child.kill();
                    break child
                        .wait()
                        .map_err(|error| format!("could not wait for the plugin: {error}"))?;
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(error) => {
                let _ = child.kill();
                return Err(format!("could not wait for the plugin: {error}"));
            }
        }
    };

    let (stdout, _) = stdout.map_or((Vec::new(), false), |receiver| {
        receiver
            .recv_timeout(READER_GRACE)
            .unwrap_or((Vec::new(), false))
    });
    let (stderr, _) = stderr.map_or((Vec::new(), false), |receiver| {
        receiver
            .recv_timeout(READER_GRACE)
            .unwrap_or((Vec::new(), false))
    });
    Ok(Captured {
        status,
        stdout,
        stderr,
        timed_out,
        overflow: overflow.load(Ordering::Relaxed),
    })
}

fn read_limited(mut pipe: impl Read, limit: usize, overflow: &AtomicBool) -> (Vec<u8>, bool) {
    let mut data = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                let room = limit.saturating_sub(data.len());
                if count <= room {
                    data.extend_from_slice(&buffer[..count]);
                } else {
                    data.extend_from_slice(&buffer[..room]);
                    overflow.store(true, Ordering::Relaxed);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    (data, overflow.load(Ordering::Relaxed))
}

fn next_request_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn default_timeout(capability: &str) -> Duration {
    if let Some(timeout) = env_timeout("DAWNGASM_PLUGIN_TIMEOUT_MS") {
        return timeout;
    }
    match capability {
        CAP_CLEARANCE => Duration::from_secs(330),
        CAP_FEED => Duration::from_secs(60),
        CAP_EXTRACT => Duration::from_secs(30),
        _ => DEFAULT_CALL_TIMEOUT,
    }
}

fn describe_timeout() -> Duration {
    env_timeout("DAWNGASM_PLUGIN_DESCRIBE_TIMEOUT_MS").unwrap_or(DEFAULT_DESCRIBE_TIMEOUT)
}

fn max_output() -> usize {
    env::var("DAWNGASM_PLUGIN_MAX_OUTPUT")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_OUTPUT)
}

fn env_timeout(name: &str) -> Option<Duration> {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_manifest_with_string_capabilities() {
        let manifest: Manifest = serde_json::from_str(
            r#"{"id":"x","name":"X","version":"1","capabilities":["extract","command"]}"#,
        )
        .unwrap();
        assert_eq!(manifest.protocol, 1);
        assert!(manifest.supports(CAP_EXTRACT));
        assert!(manifest.supports(CAP_COMMAND));
        assert_eq!(
            manifest.capability(CAP_EXTRACT).unwrap().methods(),
            vec![METHOD_EXTRACT_RESOLVE]
        );
    }

    #[test]
    fn parses_a_manifest_with_full_capabilities() {
        let manifest: Manifest = serde_json::from_str(
            r#"{
                "protocol": 2,
                "id": "x",
                "name": "X",
                "capabilities": [
                    {"name": "extract", "hosts": ["example.com"], "priority": 5, "override_builtin": true}
                ],
                "commands": [{"id": "hello", "title": "Hello"}]
            }"#,
        )
        .unwrap();
        assert_eq!(manifest.protocol, 2);
        let capability = manifest.capability(CAP_EXTRACT).unwrap();
        assert_eq!(capability.hosts, vec!["example.com"]);
        assert_eq!(capability.priority, 5);
        assert!(capability.override_builtin);
        assert_eq!(manifest.commands[0].display_title(), "Hello");
    }

    #[test]
    fn rejects_a_manifest_without_capabilities() {
        let manifest: Manifest =
            serde_json::from_str(r#"{"id":"x","name":"X","capabilities":[]}"#).unwrap();
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn matches_hosts_exactly_and_by_suffix() {
        let patterns = vec!["example.com".to_string(), "*.audio.example".to_string()];
        assert!(host_matches(&patterns, "example.com"));
        assert!(!host_matches(&patterns, "www.example.com"));
        assert!(host_matches(&patterns, "audio.example"));
        assert!(host_matches(&patterns, "cdn.audio.example"));
        assert!(!host_matches(&patterns, "notaudio.example"));
    }

    #[test]
    fn extracts_hosts_and_urls() {
        assert_eq!(
            host_of("https://user:pass@Example.COM:8443/a/b?x=1").as_deref(),
            Some("example.com")
        );
        assert_eq!(host_of("not a url"), Some("not a url".to_string()));
        let text = "listen at https://a.example/x and http://b.example/y).";
        assert_eq!(
            urls_in_text(text),
            vec!["https://a.example/x", "http://b.example/y"]
        );
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

        let failed: Response = serde_json::from_str(
            r#"{"ok":false,"error":{"code":"unsupported","message":"not my host"}}"#,
        )
        .unwrap();
        assert!(!failed.ok);
        let error = failed.error.unwrap().into_parts("test");
        assert_eq!(error.code, "unsupported");
        assert!(error.is_unhandled());

        let legacy: Response =
            serde_json::from_str(r#"{"ok":false,"error":"no clearance"}"#).unwrap();
        assert_eq!(
            legacy.error.unwrap().into_parts("test").message,
            "no clearance"
        );
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
            "#!/bin/sh\ncat >/dev/null\nprintf '{\"protocol\":2,\"ok\":true,\"result\":{\"echo\":1}}'\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let plugin = Extension {
            manifest: Manifest {
                // Version 1, so the host must omit `id` and this script does
                // not have to echo it.
                protocol: 1,
                id: "test".into(),
                name: "Test".into(),
                version: String::new(),
                description: String::new(),
                capabilities: vec![CapabilityDecl {
                    name: CAP_EXTRACT.into(),
                    ..CapabilityDecl::default()
                }],
                commands: Vec::new(),
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

    #[cfg(unix)]
    #[test]
    fn a_hanging_plugin_times_out() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = env::temp_dir().join(format!("dawngasm-plugin-timeout-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dawngasm-plugin-hang");
        fs::write(&path, "#!/bin/sh\nexec sleep 5\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let plugin = Extension {
            manifest: Manifest {
                protocol: 2,
                id: "hang".into(),
                name: "Hang".into(),
                version: String::new(),
                description: String::new(),
                capabilities: vec![CapabilityDecl {
                    name: CAP_EXTRACT.into(),
                    timeout_ms: Some(50),
                    ..CapabilityDecl::default()
                }],
                commands: Vec::new(),
            },
            path,
        };
        let error = plugin
            .call(METHOD_EXTRACT_RESOLVE, serde_json::json!({}))
            .unwrap_err();
        assert_eq!(error.code, "timeout");
        assert!(error.retryable);
        let _ = fs::remove_dir_all(&dir);
    }

    fn test_extension(id: &str, capabilities: Vec<CapabilityDecl>) -> Extension {
        Extension {
            manifest: Manifest {
                protocol: 2,
                id: id.into(),
                name: id.into(),
                version: String::new(),
                description: String::new(),
                capabilities,
                commands: Vec::new(),
            },
            path: PathBuf::from(format!("/nonexistent/dawngasm-plugin-{id}")),
        }
    }

    fn capability(name: &str, methods: &[&str], hosts: &[&str], priority: i32) -> CapabilityDecl {
        CapabilityDecl {
            name: name.into(),
            methods: methods.iter().map(|method| (*method).to_string()).collect(),
            hosts: hosts.iter().map(|host| (*host).to_string()).collect(),
            priority,
            ..CapabilityDecl::default()
        }
    }

    fn loaded(plugins: Vec<Extension>) -> Extensions {
        Extensions {
            plugins,
            failures: Vec::new(),
        }
    }

    #[test]
    fn request_ids_are_only_sent_to_protocol_two_plugins() {
        let params = serde_json::json!({});
        let version_one = serde_json::to_value(Request {
            protocol: 1,
            id: None,
            method: METHOD_EXTRACT_RESOLVE,
            params: params.clone(),
        })
        .unwrap();
        assert!(version_one.get("id").is_none());

        let version_two = serde_json::to_value(Request {
            protocol: 2,
            id: Some("1234-0"),
            method: METHOD_EXTRACT_RESOLVE,
            params,
        })
        .unwrap();
        assert_eq!(version_two["id"], "1234-0");
    }

    #[test]
    fn methods_narrow_capability_dispatch() {
        let extensions = loaded(vec![
            test_extension(
                "tags-only",
                vec![capability(CAP_FEED, &[METHOD_FEED_TAGS], &[], 0)],
            ),
            test_extension(
                "fetch-only",
                vec![capability(CAP_FEED, &[METHOD_FEED_FETCH], &[], 0)],
            ),
            test_extension("defaults", vec![capability(CAP_FEED, &[], &[], 0)]),
        ]);

        assert_eq!(
            extensions
                .find_for_method(CAP_FEED, METHOD_FEED_FETCH)
                .map(|plugin| plugin.manifest.id.as_str()),
            Some("fetch-only")
        );
        assert_eq!(
            extensions
                .find_for_method(CAP_FEED, METHOD_FEED_TAGS)
                .map(|plugin| plugin.manifest.id.as_str()),
            Some("tags-only")
        );
        assert!(extensions
            .find_for_method(CAP_FEED, "feed.unknown")
            .is_none());
    }

    #[test]
    fn dispatch_orders_by_the_used_capability_priority() {
        let extensions = loaded(vec![
            test_extension("low", vec![capability(CAP_FILTER, &[], &[], 1)]),
            test_extension("high", vec![capability(CAP_FILTER, &[], &[], 9)]),
            test_extension("middle", vec![capability(CAP_FILTER, &[], &[], 5)]),
        ]);
        let ids: Vec<_> = extensions
            .find_all_for_method(CAP_FILTER, METHOD_FILTER_APPLY)
            .into_iter()
            .map(|plugin| plugin.manifest.id.as_str())
            .collect();
        assert_eq!(ids, ["high", "middle", "low"]);
    }

    #[test]
    fn extractors_rank_exact_and_longer_suffixes_first() {
        let extensions = loaded(vec![
            test_extension(
                "broad",
                vec![capability(CAP_EXTRACT, &[], &["*.example.com"], 100)],
            ),
            test_extension(
                "narrow",
                vec![capability(CAP_EXTRACT, &[], &["*.cdn.example.com"], 0)],
            ),
            test_extension(
                "exact",
                vec![capability(CAP_EXTRACT, &[], &["cdn.example.com"], 0)],
            ),
            test_extension(
                "elsewhere",
                vec![capability(CAP_EXTRACT, &[], &["other.test"], 0)],
            ),
        ]);
        let ids: Vec<_> = extensions
            .extractors_for_urls(&["https://cdn.example.com/track"])
            .into_iter()
            .map(|plugin| plugin.manifest.id.as_str())
            .collect();
        assert_eq!(ids, ["exact", "narrow", "broad"]);
    }

    #[test]
    fn commands_respect_method_gating() {
        let mut enabled = test_extension(
            "enabled",
            vec![capability(CAP_COMMAND, &[METHOD_COMMAND_INVOKE], &[], 0)],
        );
        enabled.manifest.commands = vec![CommandDecl {
            id: "hello".into(),
            title: String::new(),
            description: String::new(),
            key: None,
        }];
        let mut disabled = test_extension(
            "disabled",
            vec![capability(CAP_COMMAND, &["command.other"], &[], 0)],
        );
        disabled.manifest.commands = vec![CommandDecl {
            id: "hidden".into(),
            title: String::new(),
            description: String::new(),
            key: None,
        }];
        let commands = loaded(vec![disabled, enabled]).commands();
        let ids: Vec<_> = commands
            .iter()
            .map(|command| command.command.id.as_str())
            .collect();
        assert_eq!(ids, ["hello"]);
    }
}
