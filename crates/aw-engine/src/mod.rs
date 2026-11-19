//! Audio sources: plain HTTP(S) (seekable via Range requests) and SoundCloud (search / resolve / stream).
pub mod hls;
pub mod http;
pub mod soundcloud;

use soundcloud::Stream;
use symphonia::core::io::{MediaSource, ReadOnlySource};

fn ext_of_url(u: &str) -> Option<String> {
    let path = u.split('?').next().unwrap_or(u);
    std::path::Path::new(path).extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase())
}

/// Open anything the player may be asked to play. Returns the byte source and a format hint (file extension).
/// Blocking: runs on the producer thread.
pub fn open_url(u: &str) -> Result<(Box<dyn MediaSource>, Option<String>), String> {
    let agent = soundcloud::agent();
    if soundcloud::is_soundcloud(u) {
        match soundcloud::stream_for_permalink(&agent, u)? {
            Stream::Progressive(url) => Ok((Box::new(http::HttpSource::open(agent, &url)?), Some("mp3".into()))),
            Stream::Hls { url, ext } => Ok((Box::new(ReadOnlySource::new(hls::HlsReader::open(agent, &url)?)), Some(ext.into()))),
        }
    } else {
        Ok((Box::new(http::HttpSource::open(agent, u)?), ext_of_url(u)))
    }
}
