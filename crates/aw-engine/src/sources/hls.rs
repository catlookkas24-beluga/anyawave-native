//! Minimal HLS (VOD) reader: downloads the segments one after another and presents them as one byte stream.
//! Works for MP3 / ADTS-AAC segments. fMP4 (`#EXT-X-MAP`) is not supported.
use std::io::{self, Read};

pub struct HlsReader {
    agent: ureq::Agent,
    segs: Vec<String>,
    idx: usize,
    cur: Option<Box<dyn Read + Send + Sync>>,
}

fn join(base: &str, rel: &str) -> String {
    if rel.starts_with("http://") || rel.starts_with("https://") { return rel.to_string(); }
    let dir = base.split('?').next().unwrap_or(base);
    match dir.rfind('/') { Some(i) => format!("{}{}", &dir[..=i], rel), None => rel.to_string() }
}

impl HlsReader {
    pub fn open(agent: ureq::Agent, playlist_url: &str) -> Result<Self, String> {
        let mut url = playlist_url.to_string();
        let mut text = agent.get(&url).call().map_err(|e| format!("hls playlist: {e}"))?.into_string().map_err(|e| e.to_string())?;
        // master playlist -> follow the first variant
        if text.contains("#EXT-X-STREAM-INF") {
            let lines: Vec<&str> = text.lines().collect();
            let mut variant = None;
            for (i, l) in lines.iter().enumerate() {
                if l.starts_with("#EXT-X-STREAM-INF") { variant = lines.get(i + 1).map(|s| s.trim().to_string()); break; }
            }
            let v = variant.ok_or("hls: empty master playlist")?;
            url = join(&url, &v);
            text = agent.get(&url).call().map_err(|e| format!("hls variant: {e}"))?.into_string().map_err(|e| e.to_string())?;
        }
        if text.contains("#EXT-X-MAP") { return Err("hls: fMP4 segments are not supported".into()); }
        let segs: Vec<String> = text.lines().map(|l| l.trim()).filter(|l| !l.is_empty() && !l.starts_with('#')).map(|l| join(&url, l)).collect();
        if segs.is_empty() { return Err("hls: no segments".into()); }
        Ok(Self { agent, segs, idx: 0, cur: None })
    }
}

impl Read for HlsReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.cur.is_none() {
                if self.idx >= self.segs.len() { return Ok(0); }
                let r = self.agent.get(&self.segs[self.idx]).call().map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
                self.idx += 1;
                self.cur = Some(r.into_reader());
            }
            let n = self.cur.as_mut().unwrap().read(buf)?;
            if n == 0 { self.cur = None; continue; }
            return Ok(n);
        }
    }
}
