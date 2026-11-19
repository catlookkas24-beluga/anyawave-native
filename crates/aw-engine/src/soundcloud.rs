//! SoundCloud via the public web API (api-v2) with a client_id scraped from the site's own scripts.
//! Unofficial: it can break whenever SoundCloud changes its site. Streams are resolved at play time (URLs expire).
use serde_json::Value;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";
static CID: Mutex<Option<(String, Instant)>> = Mutex::new(None);

pub fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(20))
        .user_agent(UA)
        .build()
}

pub fn is_soundcloud(u: &str) -> bool {
    ["https://soundcloud.com/", "https://www.soundcloud.com/", "https://m.soundcloud.com/", "https://on.soundcloud.com/"]
        .iter().any(|p| u.starts_with(p))
}

pub fn enc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { o.push(b as char); } else { o.push_str(&format!("%{:02X}", b)); }
    }
    o
}

fn get_text(a: &ureq::Agent, url: &str) -> Result<String, String> {
    a.get(url).call().map_err(|e| format!("GET {url}: {e}"))?.into_string().map_err(|e| e.to_string())
}

fn find_cid(js: &str) -> Option<String> {
    let i = js.find("client_id:\"")?;
    let id: String = js[i + 11..].chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
    if id.len() >= 20 { Some(id) } else { None }
}

fn fetch_client_id(a: &ureq::Agent) -> Result<String, String> {
    let html = get_text(a, "https://soundcloud.com/")?;
    let mut scripts: Vec<String> = Vec::new();
    let mut rest = html.as_str();
    while let Some(i) = rest.find("https://a-v2.sndcdn.com/assets/") {
        let tail = &rest[i..];
        match tail.find(".js") {
            Some(e) => { scripts.push(tail[..e + 3].to_string()); rest = &tail[e + 3..]; }
            None => break,
        }
    }
    for s in scripts.iter().rev() {
        if let Ok(js) = get_text(a, s) { if let Some(id) = find_cid(&js) { return Ok(id); } }
    }
    Err("soundcloud: client_id not found".into())
}

fn client_id(a: &ureq::Agent, force: bool) -> Result<String, String> {
    if !force {
        if let Ok(g) = CID.lock() {
            if let Some((id, t)) = g.as_ref() { if t.elapsed() < Duration::from_secs(6 * 3600) { return Ok(id.clone()); } }
        }
    }
    let id = fetch_client_id(a)?;
    if let Ok(mut g) = CID.lock() { *g = Some((id.clone(), Instant::now())); }
    Ok(id)
}

/// GET api with client_id appended; refreshes the client_id once on 401/403
fn api(a: &ureq::Agent, base: &str) -> Result<Value, String> {
    for attempt in 0..2 {
        let cid = client_id(a, attempt == 1)?;
        let sep = if base.contains('?') { '&' } else { '?' };
        match a.get(&format!("{base}{sep}client_id={cid}")).call() {
            Ok(r) => {
                let t = r.into_string().map_err(|e| e.to_string())?;
                return serde_json::from_str(&t).map_err(|e| format!("json: {e}"));
            }
            Err(ureq::Error::Status(c, _)) if (c == 401 || c == 403) && attempt == 0 => continue,
            Err(ureq::Error::Status(404, _)) => return Err("soundcloud: not found".into()),
            Err(e) => return Err(format!("soundcloud: {e}")),
        }
    }
    Err("soundcloud: request failed".into())
}

#[derive(Clone, Debug)]
pub struct ScTrack {
    pub id: u64,
    pub title: String,
    pub author: String,
    pub duration_ms: u64,
    pub permalink: String,
    pub artwork: Option<String>,
    pub preview_only: bool,
}

fn parse_track(v: &Value) -> Option<ScTrack> {
    let id = v["id"].as_u64()?;
    let title = v["title"].as_str()?.to_string();
    if v["policy"] == "BLOCK" { return None; }
    Some(ScTrack {
        id,
        title,
        author: v["user"]["username"].as_str().unwrap_or("Unknown").to_string(),
        duration_ms: v["full_duration"].as_u64().or_else(|| v["duration"].as_u64()).unwrap_or(0),
        permalink: v["permalink_url"].as_str()?.to_string(),
        artwork: v["artwork_url"].as_str().map(|s| s.replace("-large.", "-t500x500.")),
        preview_only: v["policy"] == "SNIP",
    })
}

pub fn search(q: &str, limit: usize) -> Result<Vec<ScTrack>, String> {
    let a = agent();
    let v = api(&a, &format!("https://api-v2.soundcloud.com/search/tracks?q={}&limit={}", enc(q), limit))?;
    Ok(v["collection"].as_array().map(|c| c.iter().filter_map(parse_track).collect()).unwrap_or_default())
}

pub enum Resolved { Track(ScTrack), Playlist { name: String, tracks: Vec<ScTrack> } }

pub fn resolve(url: &str) -> Result<Resolved, String> {
    let a = agent();
    let v = api(&a, &format!("https://api-v2.soundcloud.com/resolve?url={}", enc(url)))?;
    match v["kind"].as_str() {
        Some("track") => parse_track(&v).map(Resolved::Track).ok_or_else(|| "soundcloud: track unavailable".to_string()),
        Some("playlist") | Some("system-playlist") => {
            let name = v["title"].as_str().unwrap_or("Playlist").to_string();
            let items = v["tracks"].as_array().cloned().unwrap_or_default();
            // playlist entries beyond the first few are stubs ({id, kind}); fetch them in chunks, keep order
            let ids: Vec<u64> = items.iter().filter_map(|t| t["id"].as_u64()).take(100).collect();
            let mut full: std::collections::HashMap<u64, ScTrack> = std::collections::HashMap::new();
            for t in items.iter().filter_map(parse_track) { full.insert(t.id, t); }
            let missing: Vec<u64> = ids.iter().copied().filter(|i| !full.contains_key(i)).collect();
            for chunk in missing.chunks(30) {
                let list = chunk.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
                if let Ok(arr) = api(&a, &format!("https://api-v2.soundcloud.com/tracks?ids={list}")) {
                    for t in arr.as_array().into_iter().flatten().filter_map(parse_track) { full.insert(t.id, t); }
                }
            }
            let tracks: Vec<ScTrack> = ids.iter().filter_map(|i| full.remove(i)).collect();
            if tracks.is_empty() { Err("soundcloud: empty playlist".into()) } else { Ok(Resolved::Playlist { name, tracks }) }
        }
        _ => Err("soundcloud: unsupported link (only tracks and playlists)".into()),
    }
}

pub enum Stream { Progressive(String), Hls { url: String, ext: &'static str } }

/// Resolve a permalink to a playable media URL (done on the producer thread right before playback).
pub fn stream_for_permalink(a: &ureq::Agent, permalink: &str) -> Result<Stream, String> {
    let v = api(a, &format!("https://api-v2.soundcloud.com/resolve?url={}", enc(permalink)))?;
    let auth = v["track_authorization"].as_str().unwrap_or("");
    let tr = v["media"]["transcodings"].as_array().ok_or("soundcloud: no transcodings (track not streamable?)")?;
    let is_mp3 = |t: &Value| t["format"]["mime_type"].as_str().unwrap_or("").contains("mpeg");
    let chosen = tr.iter().find(|t| t["format"]["protocol"] == "progressive" && is_mp3(t))
        .or_else(|| tr.iter().find(|t| t["format"]["protocol"] == "hls" && is_mp3(t)))
        .ok_or("soundcloud: no supported stream (needs progressive or HLS mp3)")?;
    let is_hls = chosen["format"]["protocol"] == "hls";
    let url = chosen["url"].as_str().ok_or("soundcloud: transcoding without url")?;
    let j = api(a, &format!("{url}?track_authorization={}", enc(auth)))?;
    let u = j["url"].as_str().ok_or("soundcloud: no stream url")?.to_string();
    Ok(if is_hls { Stream::Hls { url: u, ext: "mp3" } } else { Stream::Progressive(u) })
}
