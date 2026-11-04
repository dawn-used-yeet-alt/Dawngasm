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

The binary is `target/release/dawngasm-tui` (`dawngasm-tui.exe` on Windows).
Linux builds need the ALSA headers. Install `libasound2-dev` on Debian/Ubuntu,
`alsa-lib-devel` on Fedora, or `alsa-lib` on Arch. The default build also uses
the system webview for hotaudio's Cloudflare clearance: install
`libwebkit2gtk-4.1-dev` on Debian/Ubuntu, `webkit2gtk4.1-devel` on Fedora, or
`webkit2gtk-4.1` on Arch. A headless build without it is available with
`cargo build --release --no-default-features`.

## Usage

Run `dawngasm-tui`. Press `?` for the full key list. `j`/`k` move, `Enter`
plays, `/` searches, `T` edits tag filters with autocomplete, `t` changes the
sort, `e` changes the time window, and `s` saves a post.

### hotaudio and Cloudflare

hotaudio is sometimes gated by a Cloudflare challenge. Press `C` to open the
track (or hotaudio's home page) in a small system-webview window. Pass the
challenge, and Dawngasm saves the resulting `cf_clearance` cookie together with
the exact browser user agent it was issued to. The pair is stored in
`hotaudio-clearance.json` in the data directory (`~/.local/share/dawngasm` on
Linux, `%APPDATA%\dawngasm` on Windows). `HOTAUDIO_COOKIE` and
`HOTAUDIO_USER_AGENT` override the saved values for a single run.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
