# Dawngasm

A terminal browser and player for audio posts on Reddit. It reads the dawngasm
index, keeps posts that resolve to a playable link, and plays tracks from
hotaudio, whyp, soundgasm, and audiochan.

## Installation

Prebuilt binaries for Linux, macOS, and Windows are on the
[releases page](https://github.com/dawn-used-yeet-alt/Dawngasm/releases). Each
archive includes the third-party licenses for its dependencies.

To build from source, install Rust 1.89 or newer and run:

```sh
git clone https://github.com/dawn-used-yeet-alt/Dawngasm.git
cd Dawngasm
cargo build --release
```

The main binary is `target/release/dawngasm-tui` (`dawngasm-tui.exe` on
Windows). Linux builds need the ALSA headers. Install `libasound2-dev` on
Debian/Ubuntu, `alsa-lib-devel` on Fedora, or `alsa-lib` on Arch.

The default build also builds `dawngasm-plugin-clearance`, the optional
webview plugin. Building it needs the WebKitGTK headers:
`libwebkit2gtk-4.1-dev` on Debian/Ubuntu, `webkit2gtk4.1-devel` on Fedora, or
`webkit2gtk-4.1` on Arch. The TUI itself does not link WebKitGTK, so a machine
without the runtime can still run it. A headless build that skips the plugin is
available with `cargo build --release --no-default-features`.

## Usage

Run `dawngasm-tui`. Press `?` for the full key list. `j`/`k` move, `Enter`
plays, `/` searches, `T` edits tag filters with autocomplete, `t` changes the
sort, `e` changes the time window, and `s` saves a post.

### hotaudio and Cloudflare

hotaudio is sometimes gated by a Cloudflare challenge. Press `C` to run the
`dawngasm-plugin-clearance` extension, which opens hotaudio's home page in a
small system-webview window. Pass the challenge, and Dawngasm saves the
resulting `cf_clearance` cookie together with the exact browser user agent it
was issued to. The pair is stored in `hotaudio-clearance.json` in the data
directory (`~/.local/share/dawngasm` on Linux, `%APPDATA%\dawngasm` on Windows).
`HOTAUDIO_COOKIE` and `HOTAUDIO_USER_AGENT` override the saved values for a
single run.

The release archive keeps the plugin in `plugins/`. Its runtime packages are
`webkit2gtk-4.1` on Arch, `libwebkit2gtk-4.1-0` on Debian/Ubuntu, and
`webkit2gtk4.1` on Fedora. Without them the rest of the app still works; `C`
reports that the webview runtime is missing.

## Extensions

Dawngasm looks for executables named `dawngasm-plugin-*` in `plugins/` next to
the binary, next to the binary itself, `<data dir>/plugins/`, and every
directory in `DAWNGASM_PLUGIN_PATH` (split like `PATH`). Extensions are separate
processes speaking JSON over stdin/stdout. Rust has no stable ABI, so this keeps
plugins independent of the compiler and crate versions used to build the TUI,
and a crashing plugin cannot take the player down.

A plugin answers `--describe` with a manifest:

```json
{"id":"clearance","name":"Cloudflare clearance","version":"0.4.0","capabilities":["clearance"]}
```

`run` reads one request and writes one response:

```json
{"protocol":1,"method":"extract.resolve","params":{"url":"https://example.com/track","selftext":"","title":"","author":""}}
{"protocol":1,"ok":true,"result":{"url":"https://cdn.example/track.m4a","headers":{"Referer":"https://example.com/"},"duration_ms":180000}}
```

Capabilities:

- `clearance`: answer `clearance.capture` with `{"cookie":"...","user_agent":"..."}`.
- `extract`: answer `extract.resolve` for a post the built-in extractors do not
  recognize. The result is a plain ranged HTTP media URL. Hotaudio remains
  built in because it needs a decrypting stream, not a URL.

If several plugins advertise the same capability, the first one found wins.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
