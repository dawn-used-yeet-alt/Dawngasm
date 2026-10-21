//! A seekable [`MediaSource`] over a decrypted hotaudio track.
//!
//! hotaudio never exposes a plaintext URL: the audio is an encrypted HAX0
//! container whose segments each need a key from an authenticated `listen`
//! API. Rather than downloading the whole thing to disk (what the desktop app
//! does) or relaying it through a localhost server, this decrypts segments on
//! demand and presents their concatenation as an ordinary seekable file, so
//! symphonia decodes it exactly like a local `.m4a`.
//!
//! The concatenated plaintext *is* the original fragmented MP4, `ftyp` + `moov`
//! and all: symphonia's isomp4 reader reads the header from segment 0 and walks
//! `moof` fragments from there, so seeks are handled by the demuxer rather than
//! by splicing the byte stream as the `GStreamer` build had to.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Seek, SeekFrom};

use hotaudio::crypto::{self, KeyMap};
use hotaudio::download;
use hotaudio::hax::{Hax0, KeyTree};
use hotaudio::http::Client;
use hotaudio::listen::{self, Handshake};
use hotaudio::range::{self, RangeSource};
use hotaudio::{block_on, Error as HotaudioError, HOTAUDIO_API_BASE, HOTAUDIO_UA};
use symphonia::core::io::MediaSource;

/// How many decrypted segments stay resident. A segment is roughly 15 s of
/// audio, and playback walks forward, so a handful is enough for smooth
/// decoding while a 30-minute track still costs a few megabytes rather than
/// the whole file.
const PLAIN_CACHE_SEGMENTS: usize = 16;

/// Bytes a Poly1305 tag adds to every stored segment. The served (plaintext)
/// space is the container space minus the header and one tag per segment.
const TAG_LEN: u64 = 16;

fn to_io(error: HotaudioError) -> io::Error {
    io::Error::other(error.to_string())
}

/// A [`RangeSource`] that owns its client and URL, so a track can hold both
/// without the self-reference `HttpRangeSource`'s borrow would need.
struct SessionRange {
    client: Client,
    url: String,
}

impl RangeSource for SessionRange {
    async fn range(&self, start: u64, end: u64) -> hotaudio::Result<Vec<u8>> {
        let response = self
            .client
            .request(
                "GET",
                &self.url,
                &[
                    ("User-Agent", HOTAUDIO_UA),
                    ("Range", &format!("bytes={start}-{end}")),
                ],
                None,
            )
            .await?;
        match response.status {
            206 => Ok(response.body),
            // A mirror that ignores `Range` sends the whole body; slice locally.
            200 => {
                let from = start as usize;
                let to = ((end + 1) as usize).min(response.body.len());
                if from >= to {
                    return Err(HotaudioError::Protocol("range start beyond body".into()));
                }
                Ok(response.body[from..to].to_vec())
            }
            other => Err(HotaudioError::Http(format!("range fetch returned {other}"))),
        }
    }
}

/// A `MediaSource` that decrypts a hotaudio container segment by segment.
pub struct HotaudioMediaSource {
    /// API client: the page handshake and key paging, with a request timeout.
    api: Client,
    handshake: Handshake,
    /// Bulk client: segment bytes, no per-request timeout.
    session: SessionRange,
    hax: Hax0,
    tree: KeyTree,
    /// Branch keys seen so far, seeded from the first listen response.
    keys: KeyMap,
    /// Memoised intermediate tree nodes; cleared whenever a page adds keys.
    cache: KeyMap,
    /// Plaintext offset of each segment across the whole track.
    plain_offsets: Vec<u64>,
    /// Plaintext length of each segment.
    plain_lengths: Vec<u64>,
    plain_len: u64,
    /// Decrypted segments, pruned in least-recently-used order.
    plain: HashMap<u32, Vec<u8>>,
    recency: VecDeque<u32>,
    pos: u64,
}

impl HotaudioMediaSource {
    /// Resolves `page_url` and opens its container, without decrypting audio.
    ///
    /// Costs the page handshake, one listen call and a read of the container
    /// header — enough to know the segment layout, which the served byte space
    /// is derived from up front.
    pub fn open(page_url: &str) -> io::Result<Self> {
        let api = Client::api();
        let handshake = block_on(listen::load_handshake(
            &api,
            page_url,
            HOTAUDIO_API_BASE,
            None,
        ))
        .map_err(to_io)?;

        let initial = block_on(listen::listen(&api, &handshake, -1, 0)).map_err(to_io)?;
        if initial.url.is_empty() {
            return Err(to_io(HotaudioError::Listen(
                "listen API returned no .hax url".into(),
            )));
        }

        let session = SessionRange {
            client: Client::bulk(),
            url: initial.url,
        };
        let (_file_length, header_length) =
            block_on(range::probe_header(&session)).map_err(to_io)?;
        if header_length == 0 {
            return Err(to_io(HotaudioError::Protocol(
                "container has an empty header".into(),
            )));
        }
        let header = block_on(session.range(0, u64::from(header_length - 1))).map_err(to_io)?;
        let hax = Hax0::parse(&header).map_err(to_io)?;
        if hax.segment_count == 0 {
            return Err(to_io(HotaudioError::Protocol(
                "container has no segments".into(),
            )));
        }

        // Plaintext space: segments concatenated, each a Poly1305 tag shorter
        // than its stored slice. Computed before any audio is decrypted so the
        // total length is known to symphonia immediately.
        let mut plain_offsets = Vec::with_capacity(hax.segment_count as usize);
        let mut plain_lengths = Vec::with_capacity(hax.segment_count as usize);
        let mut cursor = 0u64;
        for i in 0..hax.segment_count as usize {
            let (start, end) = hax.slice_range(i);
            let stored = (end - start) as u64;
            if stored <= TAG_LEN {
                return Err(to_io(HotaudioError::Protocol(format!(
                    "segment {i} is too short"
                ))));
            }
            plain_offsets.push(cursor);
            plain_lengths.push(stored - TAG_LEN);
            cursor += stored - TAG_LEN;
        }

        Ok(Self {
            api,
            handshake,
            session,
            tree: KeyTree::new(hax.segment_count),
            keys: listen::keys_to_map(&initial.keys),
            cache: KeyMap::new(),
            hax,
            plain_offsets,
            plain_lengths,
            plain_len: cursor,
            plain: HashMap::new(),
            recency: VecDeque::new(),
            pos: 0,
        })
    }

    /// The container's declared duration in milliseconds.
    #[must_use]
    pub fn duration_ms(&self) -> i64 {
        self.hax.duration_ms.min(i64::MAX as u64) as i64
    }

    /// The segment containing plaintext `offset`.
    fn segment_for_offset(&self, offset: u64) -> u32 {
        let index = self.plain_offsets.partition_point(|&start| start <= offset);
        index.saturating_sub(1) as u32
    }

    /// Marks `index` as most-recently used, evicting the oldest past capacity.
    fn touch(&mut self, index: u32) {
        if let Some(position) = self.recency.iter().position(|&i| i == index) {
            self.recency.remove(position);
        }
        self.recency.push_back(index);
        while self.recency.len() > PLAIN_CACHE_SEGMENTS.max(2) {
            if let Some(oldest) = self.recency.pop_front() {
                self.plain.remove(&oldest);
            }
        }
    }

    /// Decrypts segment `index`, paging key branches if it is not yet covered.
    fn ensure_segment(&mut self, index: u32) -> io::Result<()> {
        if self.plain.contains_key(&index) {
            self.touch(index);
            return Ok(());
        }

        // A miss means no ancestor of the leaf is known yet; `derive_or_page`
        // asks the listen API for the branch that covers it. Each attempt draws
        // a different window width, which is why a seek into a long track can
        // need more than one.
        let key = {
            let mut pager = download::ListenPager::new(&self.api, &self.handshake);
            let mut pages = 0u32;
            let tree = &self.tree;
            let keys = &mut self.keys;
            let cache = &mut self.cache;
            block_on(async move {
                download::derive_or_page(&mut pager, tree, keys, cache, index, &mut pages).await
            })
            .map_err(to_io)?
        };

        let (start, end) = self.hax.slice_range(index as usize);
        let stored = block_on(self.session.range(start as u64, (end - 1) as u64)).map_err(to_io)?;
        if stored.len() != end - start {
            return Err(to_io(HotaudioError::Protocol(format!(
                "segment {index}: expected {} bytes, got {}",
                end - start,
                stored.len()
            ))));
        }
        let plain = crypto::open_zero(&key, &stored).map_err(to_io)?;
        // The served layout is derived from the tag length, so a segment that
        // decrypts to a different size would misplace every byte after it.
        if plain.len() as u64 != self.plain_lengths[index as usize] {
            return Err(to_io(HotaudioError::Protocol(format!(
                "segment {index} decrypted to {} bytes, layout expects {}",
                plain.len(),
                self.plain_lengths[index as usize]
            ))));
        }
        self.plain.insert(index, plain);
        self.touch(index);
        Ok(())
    }
}

impl Read for HotaudioMediaSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pos >= self.plain_len {
            return Ok(0);
        }
        let index = self.segment_for_offset(self.pos);
        self.ensure_segment(index)?;
        let segment_start = self.plain_offsets[index as usize];
        let plain = self
            .plain
            .get(&index)
            .expect("ensure_segment inserted the segment");
        let offset = (self.pos - segment_start) as usize;
        let count = (plain.len() - offset).min(buf.len());
        buf[..count].copy_from_slice(&plain[offset..offset + count]);
        self.pos += count as u64;
        Ok(count)
    }
}

impl Seek for HotaudioMediaSource {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::Current(delta) => self.pos as i64 + delta,
            SeekFrom::End(delta) => self.plain_len as i64 + delta,
        };
        self.pos = target.max(0) as u64;
        Ok(self.pos)
    }
}

impl MediaSource for HotaudioMediaSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.plain_len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The offset → segment mapping is what every `read` depends on; test it
    /// against a synthetic layout without a network.
    fn offsets() -> Vec<u64> {
        vec![0, 100, 300, 600]
    }

    #[test]
    fn maps_offsets_onto_the_containing_segment() {
        let mut source = HotaudioMediaSource {
            api: Client::api(),
            handshake: listen::Handshake {
                state: listen::PageState {
                    pid: String::new(),
                    tick: String::new(),
                    key: String::new(),
                    tracks: HashMap::new(),
                    order: Vec::new(),
                },
                tid: String::new(),
                track: listen::Track {
                    key: String::new(),
                    title: String::new(),
                },
                client_pub_hex: String::new(),
                secret: [0; 32],
                api_base: String::new(),
                listen_key: None,
            },
            session: SessionRange {
                client: Client::api(),
                url: String::new(),
            },
            hax: Hax0 {
                file_length: 0,
                header_length: 0,
                extra_length: 0,
                base_key: Vec::new(),
                extra: Vec::new(),
                codec: String::new(),
                duration_ms: 0,
                segment_count: 4,
                segments: Vec::new(),
            },
            tree: KeyTree::new(4),
            keys: KeyMap::new(),
            cache: KeyMap::new(),
            plain_offsets: offsets(),
            plain_lengths: vec![100, 200, 300, 100],
            plain_len: 700,
            plain: HashMap::new(),
            recency: VecDeque::new(),
            pos: 0,
        };
        assert_eq!(source.segment_for_offset(0), 0);
        assert_eq!(source.segment_for_offset(99), 0);
        assert_eq!(source.segment_for_offset(100), 1);
        assert_eq!(source.segment_for_offset(299), 1);
        assert_eq!(source.segment_for_offset(300), 2);
        assert_eq!(source.segment_for_offset(699), 3);
        assert_eq!(source.segment_for_offset(700), 3);

        // The LRU cap keeps at most the configured number of segments.
        source.plain_len = 700;
        for index in 0..PLAIN_CACHE_SEGMENTS as u32 + 5 {
            source.plain.insert(index, vec![0u8; 1]);
            source.touch(index);
        }
        assert!(source.plain.len() <= PLAIN_CACHE_SEGMENTS);
    }
}
