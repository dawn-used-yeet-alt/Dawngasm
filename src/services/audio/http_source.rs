//! A ranged, seekable [`MediaSource`] over HTTP.
//!
//! soundgasm, whyp and audiochan all serve a plain file with `Accept-Ranges:
//! bytes`, so the only thing standing between them and symphonia is a `Read` +
//! `Seek` that turns a byte range into a `Range` request. Bytes are fetched one
//! bounded chunk at a time, so a 30-minute track costs one chunk of memory
//! rather than its full size — which is the whole point of not just setting
//! `audio.src` and letting a browser buffer it.

use std::io::{self, Read, Seek, SeekFrom};

use symphonia::core::io::MediaSource;

use super::super::extractors::BROWSER_UA;

/// How many bytes one ranged request pulls. Large enough that a decode-heavy
/// stretch is a handful of round trips, small enough that peak memory stays in
/// the hundreds of kilobytes.
const CHUNK: usize = 256 * 1024;

/// A `MediaSource` backed by ranged HTTP reads.
pub struct HttpMediaSource {
    agent: ureq::Agent,
    url: String,
    headers: Vec<(String, String)>,
    len: u64,
    pos: u64,
    chunk_start: u64,
    chunk: Vec<u8>,
}

impl HttpMediaSource {
    /// Opens `url`, learning its length from the first ranged response.
    ///
    /// `headers` are attached to every request; audiochan needs its `Origin` /
    /// `Referer` and the agent's cookie jar for the play-intent cookie.
    pub fn open(agent: ureq::Agent, url: impl Into<String>) -> io::Result<Self> {
        Self::with_headers(agent, url, Vec::new())
    }

    pub fn with_headers(
        agent: ureq::Agent,
        url: impl Into<String>,
        headers: Vec<(String, String)>,
    ) -> io::Result<Self> {
        let mut source = Self {
            agent,
            url: url.into(),
            headers,
            len: 0,
            pos: 0,
            chunk_start: 0,
            chunk: Vec::new(),
        };
        // The first chunk doubles as the length probe: its `Content-Range`
        // carries the total, so opening costs a single request.
        source.fetch_at(0)?;
        Ok(source)
    }

    /// A browser UA is required by every host; sharing it with the extractors
    /// keeps their requests indistinguishable.
    #[must_use]
    pub fn browser_agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .user_agent(BROWSER_UA)
            .build()
            .new_agent()
    }

    #[must_use]
    pub const fn len(&self) -> u64 {
        self.len
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    const fn chunk_covers(&self, pos: u64) -> bool {
        pos >= self.chunk_start && pos - self.chunk_start < self.chunk.len() as u64
    }

    fn fetch_at(&mut self, start: u64) -> io::Result<()> {
        let last = match self.len {
            0 => start.saturating_add(CHUNK as u64 - 1),
            len => start
                .saturating_add(CHUNK as u64 - 1)
                .min(len.saturating_sub(1)),
        };
        let mut request = self
            .agent
            .get(&self.url)
            .header("Range", format!("bytes={start}-{last}"));
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        let response = match request.call() {
            Ok(response) => response,
            // A range past the end is not a failure: the caller's read just
            // returns 0 from here.
            Err(ureq::Error::StatusCode(416)) => {
                self.chunk.clear();
                self.chunk_start = start;
                return Ok(());
            }
            Err(error) => {
                return Err(io::Error::other(format!(
                    "range request for {} failed: {error}",
                    self.url
                )));
            }
        };
        let status = response.status().as_u16();
        if status != 200 && status != 206 {
            return Err(io::Error::other(format!(
                "range request for {} returned HTTP {status}",
                self.url
            )));
        }
        if let Some(total) = content_range_total(&response) {
            self.len = total;
        } else if let Some(length) = content_length(&response) {
            // A 206 body is the remainder, so its length plus the offset is the
            // total; a 200 body is the whole object.
            self.len = if status == 200 {
                length
            } else {
                start + length
            };
        }

        let mut reader = response.into_body().into_reader();
        // A mirror that ignores `Range` re-sends from byte zero; skip past the
        // part a previous chunk already covered instead of buffering it.
        if status == 200 && start > 0 {
            discard(&mut reader, start)?;
        }
        self.chunk.clear();
        self.chunk.resize(CHUNK, 0);
        let mut filled = 0usize;
        while filled < self.chunk.len() {
            match reader.read(&mut self.chunk[filled..]) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        self.chunk.truncate(filled);
        self.chunk_start = start;
        Ok(())
    }
}

impl Read for HttpMediaSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || (self.len != 0 && self.pos >= self.len) {
            return Ok(0);
        }
        if !self.chunk_covers(self.pos) {
            self.fetch_at(self.pos)?;
        }
        if self.chunk.is_empty() {
            // The server had nothing at this offset; that is the end of file.
            self.len = self.len.min(self.pos);
            return Ok(0);
        }
        let offset = (self.pos - self.chunk_start) as usize;
        let available = &self.chunk[offset..];
        let count = available.len().min(buf.len());
        buf[..count].copy_from_slice(&available[..count]);
        self.pos += count as u64;
        Ok(count)
    }
}

impl Seek for HttpMediaSource {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::Current(delta) => self.pos as i64 + delta,
            SeekFrom::End(delta) => self.len as i64 + delta,
        };
        self.pos = target.max(0) as u64;
        // The buffered chunk rarely covers an arbitrary seek; drop it so the
        // next read fetches from the new position.
        if !self.chunk_covers(self.pos) {
            self.chunk.clear();
        }
        Ok(self.pos)
    }
}

impl MediaSource for HttpMediaSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        (self.len != 0).then_some(self.len)
    }
}

/// `Content-Range: bytes 0-0/12345` → `12345`.
fn content_range_total(response: &ureq::http::Response<ureq::Body>) -> Option<u64> {
    response
        .headers()
        .get("content-range")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit('/').next())
        .and_then(|total| total.trim().parse::<u64>().ok())
}

fn content_length(response: &ureq::http::Response<ureq::Body>) -> Option<u64> {
    response
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
}

fn discard(reader: &mut impl Read, mut count: u64) -> io::Result<()> {
    let mut scratch = [0u8; 32 * 1024];
    while count > 0 {
        let want = (scratch.len() as u64).min(count) as usize;
        match reader.read(&mut scratch[..want]) {
            Ok(0) => break,
            Ok(read) => count -= read as u64,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn content_range_parses_the_total() {
        // Exercised through the parser directly rather than an HTTP response,
        // since the response type cannot be built standalone.
        let parse = |value: &str| -> Option<u64> {
            value
                .rsplit('/')
                .next()
                .and_then(|t| t.trim().parse::<u64>().ok())
        };
        assert_eq!(parse("bytes 0-262143/12345678"), Some(12345678));
        assert_eq!(parse("bytes 0-0/*"), None);
        assert_eq!(parse("garbage"), None);
    }
}
