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
`alsa-lib-devel` on Fedora, or `alsa-lib` on Arch.

## Usage

Run `dawngasm-tui`. Press `?` for the full key list. `j`/`k` move, `Enter`
plays, `/` searches, `T` edits tag filters with autocomplete, `t` changes the
sort, `e` changes the time window, and `s` saves a post.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
