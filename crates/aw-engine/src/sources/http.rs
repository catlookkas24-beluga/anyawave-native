//! HTTP source with real seeking: every seek re-opens the stream with a `Range:` request.
//! Servers that ignore Range are still playable, just not seekable.
use std::io::{self, Read, Seek, SeekFrom};
use symphonia::core::io::MediaSource;

pub struct HttpSource {
    agent: ureq::Agent,
    url: String,
    len: Option<u64>,
    seekable: bool,
    pos: u64,
    rd: Option<Box<dyn Read + Send + Sync>>,
}

impl HttpSource {
    pub fn open(agent: ureq::Agent, url: &str) -> Result<Self, String> {
        let resp = agent.get(url).set("Range", "bytes=0-").call().map_err(|e| format!("http: {e}"))?;
        let (len, seekable) = if resp.status() == 206 {
            // Content-Range: bytes 0-9/100
            let total = resp.header("Content-Range").and_then(|h| h.rsplit('/').next()).and_then(|t| t.trim().parse::<u64>().ok());
            (total, total.is_some())
        } else {
            (resp.header("Content-Length").and_then(|h| h.trim().parse::<u64>().ok()), false)
        };
        let rd = resp.into_reader();
        Ok(Self { agent, url: url.to_string(), len, seekable, pos: 0, rd: Some(rd) })
    }
}

impl Read for HttpSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.rd.is_none() {
            if !self.seekable && self.pos > 0 { return Ok(0); }
            if let Some(l) = self.len { if self.pos >= l { return Ok(0); } }
            let r = self.agent.get(&self.url).set("Range", &format!("bytes={}-", self.pos)).call()
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            if self.pos > 0 && r.status() != 206 { return Err(io::Error::new(io::ErrorKind::Other, "range not honoured")); }
            self.rd = Some(r.into_reader());
        }
        let n = self.rd.as_mut().unwrap().read(buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for HttpSource {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        if !self.seekable { return Err(io::Error::new(io::ErrorKind::Unsupported, "source is not seekable")); }
        let len = self.len.unwrap_or(0) as i64;
        let target = match to { SeekFrom::Start(p) => p as i64, SeekFrom::Current(d) => self.pos as i64 + d, SeekFrom::End(d) => len + d };
        if target < 0 { return Err(io::Error::new(io::ErrorKind::InvalidInput, "negative seek")); }
        let t = target as u64;
        if t != self.pos { self.rd = None; self.pos = t; }
        Ok(self.pos)
    }
}

impl MediaSource for HttpSource {
    fn is_seekable(&self) -> bool { self.seekable }
    fn byte_len(&self) -> Option<u64> { self.len }
}
