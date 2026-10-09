//! Detecting new versions of a source and downloading them.
//!
//! - Geofabrik URLs (`…-latest.osm.pbf`): the replication `state.txt` next to
//!   the file gives a sequence number and data timestamp; `.md5` verifies
//!   the download.
//! - Other URLs: `ETag` / `Last-Modified` / `Content-Length`, plus a `.md5`
//!   sidecar when the server has one.
//! - Local files: modification time and size.

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use md5::{Digest, Md5};
use tracing::{debug, info};

use crate::source::Version;

pub struct Http {
    agent: ureq::Agent,
}

impl Default for Http {
    fn default() -> Self {
        Self::new()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Geofabrik publishes `<region>-updates/state.txt` next to `<region>-latest.osm.pbf`.
pub fn geofabrik_state_url(url: &str) -> Option<String> {
    let host = url.split("://").nth(1)?.split('/').next()?;
    if !host.ends_with("geofabrik.de") {
        return None;
    }
    let base = url.strip_suffix("-latest.osm.pbf")?;
    Some(format!("{base}-updates/state.txt"))
}

/// Parse an osmosis replication `state.txt`.
pub fn parse_state(text: &str) -> (Option<u64>, Option<String>) {
    let mut seq = None;
    let mut ts = None;
    for line in text.lines().map(str::trim).filter(|l| !l.starts_with('#')) {
        match line.split_once('=') {
            Some(("sequenceNumber", v)) => seq = v.trim().parse().ok(),
            Some(("timestamp", v)) => ts = Some(v.trim().replace("\\:", ":")),
            _ => {}
        }
    }
    (seq, ts)
}

/// First token of a `.md5` file, if it looks like an MD5 hash.
fn parse_md5(text: &str) -> Option<String> {
    let h = text.split_whitespace().next()?.to_ascii_lowercase();
    (h.len() == 32 && h.chars().all(|c| c.is_ascii_hexdigit())).then_some(h)
}

pub fn check_local(path: &Path) -> Result<Version> {
    let meta =
        std::fs::metadata(path).with_context(|| format!("cannot read '{}'", path.display()))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    Ok(Version {
        mtime,
        size: Some(meta.len()),
        ..Default::default()
    })
}

impl Http {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .user_agent(concat!("geors/", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(60)))
            .max_redirects(10)
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
        }
    }

    /// GET a small text resource; `None` if it does not exist.
    fn get_text(&self, url: &str) -> Result<Option<String>> {
        match self.agent.get(url).call() {
            Ok(mut resp) => Ok(Some(resp.body_mut().read_to_string()?)),
            Err(ureq::Error::StatusCode(404 | 403 | 410)) => Ok(None),
            Err(e) => Err(e).with_context(|| format!("GET {url}")),
        }
    }

    fn md5_sidecar(&self, url: &str) -> Option<String> {
        match self.get_text(&format!("{url}.md5")) {
            Ok(Some(t)) => parse_md5(&t),
            Ok(None) => None,
            Err(e) => {
                debug!(error = %e, "no md5 sidecar");
                None
            }
        }
    }

    /// Current published version of `url`.
    pub fn check(&self, url: &str) -> Result<Version> {
        let mut v = Version::default();
        if let Some(state_url) = geofabrik_state_url(url)
            && let Some(text) = self.get_text(&state_url)?
        {
            (v.sequence, v.timestamp) = parse_state(&text);
        }
        if v.sequence.is_none() {
            let resp = self
                .agent
                .head(url)
                .call()
                .with_context(|| format!("HEAD {url}"))?;
            let header = |name: &str| {
                resp.headers()
                    .get(name)
                    .and_then(|h| h.to_str().ok())
                    .map(str::to_string)
            };
            v.etag = header("etag");
            v.last_modified = header("last-modified");
            v.size = header("content-length").and_then(|s| s.parse().ok());
        }
        v.md5 = self.md5_sidecar(url);
        Ok(v)
    }

    /// Stream `url` to `dest`, verifying `expected_md5` if given. Returns the
    /// MD5 of the downloaded file.
    pub fn download(&self, url: &str, dest: &Path, expected_md5: Option<&str>) -> Result<String> {
        let t = Instant::now();
        let mut resp = self
            .agent
            .get(url)
            .call()
            .with_context(|| format!("GET {url}"))?;
        let total: Option<u64> = resp
            .headers()
            .get("content-length")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse().ok());
        info!(url, size_mb = total.map(|b| b / 1_048_576), "downloading");
        let part = dest.with_extension("part");
        let mut out = BufWriter::new(
            File::create(&part).with_context(|| format!("cannot create '{}'", part.display()))?,
        );
        let mut reader = resp.body_mut().as_reader();
        let mut hasher = Md5::new();
        let mut buf = vec![0u8; 256 * 1024];
        let mut done = 0u64;
        let mut last_log = Instant::now();
        loop {
            let n = reader
                .read(&mut buf)
                .with_context(|| format!("download of {url} interrupted"))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])?;
            done += n as u64;
            if last_log.elapsed() > Duration::from_secs(5) {
                last_log = Instant::now();
                match total {
                    Some(t) if t > 0 => info!(
                        "downloaded {:.0}% ({} MiB)",
                        done as f64 * 100.0 / t as f64,
                        done / 1_048_576
                    ),
                    _ => info!("downloaded {} MiB", done / 1_048_576),
                }
            }
        }
        out.flush()?;
        drop(out);
        if let Some(t) = total
            && t != done
        {
            let _ = std::fs::remove_file(&part);
            bail!("download of {url} incomplete: got {done} of {t} bytes");
        }
        let md5 = hex(&hasher.finalize());
        if let Some(expected) = expected_md5
            && expected != md5
        {
            let _ = std::fs::remove_file(&part);
            bail!("checksum mismatch for {url}: expected {expected}, got {md5}");
        }
        std::fs::rename(&part, dest)?;
        info!(url, mb = done / 1_048_576, elapsed = ?t.elapsed(), "download complete");
        Ok(md5)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geofabrik_urls() {
        assert_eq!(
            geofabrik_state_url(
                "https://download.geofabrik.de/europe/liechtenstein-latest.osm.pbf"
            )
            .as_deref(),
            Some("https://download.geofabrik.de/europe/liechtenstein-updates/state.txt")
        );
        assert_eq!(
            geofabrik_state_url("https://example.com/liechtenstein-latest.osm.pbf"),
            None
        );
        assert_eq!(
            geofabrik_state_url(
                "https://download.geofabrik.de/europe/liechtenstein-260101.osm.pbf"
            ),
            None
        );
    }

    #[test]
    fn state_and_md5_parsing() {
        let text = "# original OSM minutely replication sequence number 7320604\ntimestamp=2026-10-08T20\\:21\\:06Z\nsequenceNumber=4932\n";
        assert_eq!(
            parse_state(text),
            (Some(4932), Some("2026-10-08T20:21:06Z".into()))
        );
        assert_eq!(
            parse_md5("1fb45d46683dfb4f72032433c33de832  liechtenstein-latest.osm.pbf\n")
                .as_deref(),
            Some("1fb45d46683dfb4f72032433c33de832")
        );
        assert_eq!(parse_md5("<html>"), None);
    }
}
