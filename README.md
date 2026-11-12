# Dawngasm

A terminal browser and player for audio posts on Reddit. It reads the dawngasm
index, keeps posts that resolve to a playable link, and plays tracks from
hotaudio, whyp, soundgasm, and audiochan.

# Android/iOS

I personally mostly use the mobile app so it's more frequently updated, up on features and (imo) looks and works much better.
You can check it out [here](https://github.com/dawn-used-yeet-alt/Dawngasm-Android).
I'd recommend the mobile app to this TUI app.

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


## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
