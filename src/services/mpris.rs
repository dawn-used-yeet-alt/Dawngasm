//! MPRIS D-Bus interface, so desktop media keys and panels can drive playback.
//!
//! Unlike the desktop build, the audio engine here lives on the UI thread and
//! its seek/pause methods take `&mut self`, so it cannot be shared directly with
//! zbus' executor. Instead this module owns two things:
//!
//! - a [`Snapshot`] of the playback state that the UI thread refreshes each
//!   tick and the interface reads synchronously for property queries;
//! - a channel of [`Command`]s that the interface fills in and the UI thread
//!   drains, so every mutation still happens on the thread that owns the engine.
//!
//! Registration is best-effort: a machine with no session bus, or one where the
//! name is already claimed, simply gets no media-key integration.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zbus::zvariant::{ObjectPath, Value};

use crate::models::post::Post;

const BUS_NAME: &str = "org.mpris.MediaPlayer2.audiogasm";
const OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";
const TRACK_PATH_PREFIX: &str = "/com/audiogasm/track/";

/// A request from a media key or a panel, applied on the UI thread.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Command {
    Play,
    Pause,
    Toggle,
    Stop,
    Next,
    Previous,
    /// Move the playhead by a signed number of milliseconds.
    SeekRelative(i64),
    /// Jump to an absolute millisecond position.
    SeekAbsolute(i64),
    SetVolume(f32),
    Quit,
}

/// Playback state as MPRIS reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackStatus {
    Stopped,
    Playing,
    Paused,
}

impl PlaybackStatus {
    fn as_str(self) -> &'static str {
        match self {
            PlaybackStatus::Stopped => "Stopped",
            PlaybackStatus::Playing => "Playing",
            PlaybackStatus::Paused => "Paused",
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            PlaybackStatus::Stopped => 0,
            PlaybackStatus::Playing => 1,
            PlaybackStatus::Paused => 2,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => PlaybackStatus::Playing,
            2 => PlaybackStatus::Paused,
            _ => PlaybackStatus::Stopped,
        }
    }
}

/// The playback state the interface reads, written by the UI thread.
struct Snapshot {
    status: AtomicU8,
    position_ms: AtomicI64,
    duration_ms: AtomicI64,
    /// `f32` bits, since there is no atomic float.
    volume_bits: AtomicU32,
    can_seek: AtomicBool,
    track: Mutex<Option<Post>>,
}

impl Snapshot {
    fn new() -> Self {
        Snapshot {
            status: AtomicU8::new(PlaybackStatus::Stopped.to_u8()),
            position_ms: AtomicI64::new(0),
            duration_ms: AtomicI64::new(0),
            volume_bits: AtomicU32::new(1.0f32.to_bits()),
            can_seek: AtomicBool::new(false),
            track: Mutex::new(None),
        }
    }

    fn status(&self) -> PlaybackStatus {
        PlaybackStatus::from_u8(self.status.load(Ordering::Relaxed))
    }

    fn position_ms(&self) -> i64 {
        self.position_ms.load(Ordering::Relaxed)
    }

    fn duration_ms(&self) -> i64 {
        self.duration_ms.load(Ordering::Relaxed)
    }

    fn volume(&self) -> f32 {
        f32::from_bits(self.volume_bits.load(Ordering::Relaxed))
    }

    fn can_seek(&self) -> bool {
        self.can_seek.load(Ordering::Relaxed)
    }

    fn track(&self) -> Option<Post> {
        self.track.lock().ok().and_then(|track| track.clone())
    }

    #[allow(clippy::too_many_arguments)]
    fn update(
        &self,
        status: PlaybackStatus,
        position_ms: i64,
        duration_ms: i64,
        volume: f32,
        can_seek: bool,
        track: Option<&Post>,
    ) {
        self.status.store(status.to_u8(), Ordering::Relaxed);
        self.position_ms
            .store(position_ms.max(0), Ordering::Relaxed);
        self.duration_ms
            .store(duration_ms.max(0), Ordering::Relaxed);
        self.volume_bits.store(volume.to_bits(), Ordering::Relaxed);
        self.can_seek.store(can_seek, Ordering::Relaxed);
        if let Ok(mut slot) = self.track.lock() {
            if slot.as_ref().map(Post::key) != track.map(Post::key) {
                *slot = track.cloned();
            }
        }
    }

    /// The properties whose change MPRIS clients are told about. `position` is
    /// deliberately excluded: the spec says not to signal it.
    fn signal_state(&self) -> SignalState {
        SignalState {
            status: self.status.load(Ordering::Relaxed),
            duration_ms: self.duration_ms(),
            volume_bits: self.volume_bits.load(Ordering::Relaxed),
            can_seek: self.can_seek(),
            track_key: self.track().map(|post| post.key().to_string()),
        }
    }
}

/// A cheap fingerprint of the signal-worthy properties, compared to decide
/// whether a `PropertiesChanged` broadcast is due.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SignalState {
    status: u8,
    duration_ms: i64,
    volume_bits: u32,
    can_seek: bool,
    track_key: Option<String>,
}

/// A live MPRIS registration, or `None` if zbus could not be started.
pub struct Mpris {
    snapshot: Arc<Snapshot>,
    commands: Receiver<Command>,
}

impl Mpris {
    /// Claims the MPRIS bus name on a background thread.
    ///
    /// Returns an inert handle if the zbus thread cannot be spawned; the caller
    /// keeps working either way.
    pub fn start() -> Option<Mpris> {
        let snapshot = Arc::new(Snapshot::new());
        let (sender, commands) = mpsc::channel();
        let thread_snapshot = Arc::clone(&snapshot);
        let spawned = std::thread::Builder::new()
            .name("audiogasm-mpris".into())
            .spawn(move || {
                if let Err(error) = serve(thread_snapshot, sender) {
                    // A bare TTY or an SSH login usually has no session bus, and
                    // printing there would scribble over the TUI; only report a
                    // failure when a bus was actually expected.
                    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some() {
                        eprintln!("mpris: not available: {error}");
                    }
                }
            })
            .is_ok();
        spawned.then_some(Mpris { snapshot, commands })
    }

    /// Refreshes what the interface reports. Called once per UI tick.
    pub fn update(
        &self,
        status: PlaybackStatus,
        position_ms: i64,
        duration_ms: i64,
        volume: f32,
        can_seek: bool,
        track: Option<&Post>,
    ) {
        self.snapshot
            .update(status, position_ms, duration_ms, volume, can_seek, track);
    }

    /// Drains requests made since the last tick.
    pub fn poll(&self) -> Vec<Command> {
        let mut commands = Vec::new();
        while let Ok(command) = self.commands.try_recv() {
            commands.push(command);
        }
        commands
    }
}

fn serve(snapshot: Arc<Snapshot>, commands: Sender<Command>) -> zbus::Result<()> {
    async_io::block_on(async move {
        let connection = zbus::Connection::session().await?;
        connection
            .object_server()
            .at(
                OBJECT_PATH,
                MprisRoot {
                    commands: Mutex::new(commands.clone()),
                },
            )
            .await?;
        connection
            .object_server()
            .at(
                OBJECT_PATH,
                MprisPlayer {
                    snapshot: Arc::clone(&snapshot),
                    commands: Mutex::new(commands),
                },
            )
            .await?;
        connection.request_name(BUS_NAME).await?;
        watch(connection, snapshot).await
    })
}

/// Broadcasts `PropertiesChanged` whenever the snapshot moves.
///
/// zbus only sends these when the interface asks it to, and panels track the
/// status, metadata and volume through them, so without this a now-playing
/// widget would sit on whatever it read at startup.
async fn watch(connection: zbus::Connection, snapshot: Arc<Snapshot>) -> zbus::Result<()> {
    let mut last = snapshot.signal_state();
    loop {
        async_io::Timer::after(Duration::from_millis(300)).await;
        let now = snapshot.signal_state();
        if now == last {
            continue;
        }
        let iface_ref = connection
            .object_server()
            .interface::<_, MprisPlayer>(OBJECT_PATH)
            .await?;
        let iface = iface_ref.get().await;
        if now.status != last.status {
            iface
                .playback_status_changed(iface_ref.signal_emitter())
                .await?;
        }
        if now.track_key != last.track_key || now.duration_ms != last.duration_ms {
            iface.metadata_changed(iface_ref.signal_emitter()).await?;
        }
        if now.volume_bits != last.volume_bits {
            iface.volume_changed(iface_ref.signal_emitter()).await?;
        }
        if now.can_seek != last.can_seek {
            iface.can_seek_changed(iface_ref.signal_emitter()).await?;
        }
        last = now;
    }
}

/// The root interface: application properties and the Quit action.
struct MprisRoot {
    commands: Mutex<Sender<Command>>,
}

impl MprisRoot {
    fn send(&self, command: Command) {
        if let Ok(sender) = self.commands.lock() {
            let _ = sender.send(command);
        }
    }
}

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl MprisRoot {
    #[zbus(property)]
    fn can_quit(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_raise(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn identity(&self) -> String {
        "Audiogasm".into()
    }
    #[zbus(property)]
    fn desktop_entry(&self) -> String {
        "com.audiogasm.Audiogasm".into()
    }
    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        vec!["https".into(), "http".into()]
    }
    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        vec!["audio/mp4".into(), "audio/m4a".into()]
    }
    fn raise(&self) {}
    fn quit(&self) {
        self.send(Command::Quit);
    }
}

/// The player interface: transport, position, volume and metadata.
struct MprisPlayer {
    snapshot: Arc<Snapshot>,
    commands: Mutex<Sender<Command>>,
}

impl MprisPlayer {
    fn send(&self, command: Command) {
        if let Ok(sender) = self.commands.lock() {
            let _ = sender.send(command);
        }
    }

    fn track_path(&self) -> Option<ObjectPath<'static>> {
        let post = self.snapshot.track()?;
        let id = if post.id.is_empty() {
            "0".to_string()
        } else {
            post.id.clone()
        };
        ObjectPath::try_from(format!("{TRACK_PATH_PREFIX}{id}")).ok()
    }
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl MprisPlayer {
    #[zbus(property)]
    fn playback_status(&self) -> String {
        self.snapshot.status().as_str().into()
    }

    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }
    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.0
    }
    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.0
    }
    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_play(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_pause(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_seek(&self) -> bool {
        self.snapshot.can_seek()
    }
    #[zbus(property)]
    fn can_control(&self) -> bool {
        true
    }

    /// Microseconds, which is what MPRIS asks for.
    #[zbus(property)]
    fn position(&self) -> i64 {
        self.snapshot.position_ms().saturating_mul(1000)
    }

    #[zbus(property)]
    fn volume(&self) -> f64 {
        self.snapshot.volume() as f64
    }

    #[zbus(property)]
    fn set_volume(&self, volume: f64) {
        self.send(Command::SetVolume(volume as f32));
    }

    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, Value<'static>> {
        let mut map: HashMap<String, Value<'static>> = HashMap::new();
        let Some(post) = self.snapshot.track() else {
            return map;
        };
        if let Some(path) = self.track_path() {
            map.insert("mpris:trackid".into(), Value::from(path));
        }
        map.insert("xesam:title".into(), Value::from(post.title));
        map.insert("xesam:artist".into(), Value::from(vec![post.author]));
        map.insert("xesam:album".into(), Value::from("r/gonewildaudio"));
        map.insert(
            "mpris:length".into(),
            Value::from(self.snapshot.duration_ms().saturating_mul(1000)),
        );
        map
    }

    fn next(&self) {
        self.send(Command::Next);
    }

    fn previous(&self) {
        self.send(Command::Previous);
    }

    fn pause(&self) {
        self.send(Command::Pause);
    }

    fn play_pause(&self) {
        self.send(Command::Toggle);
    }

    fn stop(&self) {
        self.send(Command::Stop);
    }

    fn play(&self) {
        self.send(Command::Play);
    }

    fn seek(&self, offset_us: i64) {
        self.send(Command::SeekRelative(offset_us / 1000));
    }

    fn set_position(&self, track_id: ObjectPath<'_>, position_us: i64) {
        // Only honour a seek for the track that is actually loaded.
        if Some(track_id) != self.track_path() {
            return;
        }
        self.send(Command::SeekAbsolute((position_us / 1000).max(0)));
    }

    fn open_uri(&self, _uri: String) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(name: &str, id: &str) -> Post {
        Post {
            id: id.into(),
            name: name.into(),
            title: "title".into(),
            author: "someone".into(),
            url: "https://example.com".into(),
            ..Default::default()
        }
    }

    #[test]
    fn status_round_trips_through_its_wire_value() {
        for status in [
            PlaybackStatus::Stopped,
            PlaybackStatus::Playing,
            PlaybackStatus::Paused,
        ] {
            assert_eq!(PlaybackStatus::from_u8(status.to_u8()), status);
        }
        assert_eq!(PlaybackStatus::from_u8(200), PlaybackStatus::Stopped);
    }

    #[test]
    fn snapshot_reflects_updates_for_the_interface() {
        let snapshot = Snapshot::new();
        assert_eq!(snapshot.status(), PlaybackStatus::Stopped);
        let track = post("t3_a", "a");
        snapshot.update(
            PlaybackStatus::Playing,
            1_500,
            4_000,
            0.5,
            true,
            Some(&track),
        );
        assert_eq!(snapshot.status(), PlaybackStatus::Playing);
        assert_eq!(snapshot.position_ms(), 1_500);
        assert_eq!(snapshot.duration_ms(), 4_000);
        assert_eq!(snapshot.volume(), 0.5);
        assert!(snapshot.can_seek());
        assert_eq!(
            snapshot.track().map(|post| post.key().to_string()),
            Some("t3_a".into())
        );
    }

    #[test]
    fn snapshot_forgets_the_track_when_none_is_given() {
        let snapshot = Snapshot::new();
        snapshot.update(
            PlaybackStatus::Playing,
            0,
            0,
            1.0,
            false,
            Some(&post("t3_a", "a")),
        );
        snapshot.update(PlaybackStatus::Stopped, 0, 0, 1.0, false, None);
        assert!(snapshot.track().is_none());
        assert_eq!(snapshot.status(), PlaybackStatus::Stopped);
    }

    #[test]
    fn signal_state_ignores_position_but_tracks_the_rest() {
        let snapshot = Snapshot::new();
        let stopped = snapshot.signal_state();

        snapshot.update(PlaybackStatus::Playing, 10, 0, 1.0, false, None);
        let playing = snapshot.signal_state();
        assert_ne!(stopped, playing, "status changes are signal-worthy");

        // Advancing the playhead must not look like a property change, or panels
        // would be spammed once per tick.
        snapshot.update(PlaybackStatus::Playing, 20, 0, 1.0, false, None);
        assert_eq!(playing, snapshot.signal_state());

        snapshot.update(
            PlaybackStatus::Playing,
            20,
            0,
            1.0,
            false,
            Some(&post("t3_a", "a")),
        );
        let with_track = snapshot.signal_state();
        assert_ne!(playing, with_track, "a new track is signal-worthy");

        snapshot.update(PlaybackStatus::Playing, 20, 0, 0.25, false, None);
        assert_ne!(
            with_track,
            snapshot.signal_state(),
            "volume is signal-worthy"
        );
    }

    #[test]
    fn negative_positions_are_clamped() {
        let snapshot = Snapshot::new();
        snapshot.update(PlaybackStatus::Playing, -100, -1, 1.0, false, None);
        assert_eq!(snapshot.position_ms(), 0);
        assert_eq!(snapshot.duration_ms(), 0);
    }
}
