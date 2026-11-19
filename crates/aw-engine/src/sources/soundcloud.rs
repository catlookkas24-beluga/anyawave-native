//! SoundCloud via the public web API (api-v2) with a client_id scraped from the site's own scripts.
//! Unofficial: it can break whenever SoundCloud changes its site. Streams are resolved at play time (URLs expire).
//! Short links (on.soundcloud.com, m.soundcloud.com) are followed to the canonical page first.
use serde_json::{json, Value};
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

fn normalize(u: &str) -> String {
    let s = u.trim();
    if s.starts_with("http://") || s.starts_with("https://") { s.to_string() } else { format!("https://{s}") }
}

fn strip_query(u: &str) -> String {
    u.split(|c| c == '?' || c == '#').next().unwrap_or(u).to_string()
}

/// lowercase host of a URL, with or without scheme
pub fn host_of(u: &str) -> Option<String> {
    let s = u.trim();
    let s = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")).unwrap_or(s);
    let host = s.split(|c| c == '/' || c == '?' || c == '#').next()?;
    Some(host.to_ascii_lowercase())
}

/// true for soundcloud.com and any of its subdomains (on., m., www.), with or without https://
pub fn is_soundcloud(u: &str) -> bool {
    host_of(u).map(|h| h == "soundcloud.com" || h.ends_with(".soundcloud.com")).unwrap_or(false)
}

/// Short links redirect to the real track/playlist page. Follow the redirect and keep scheme+host+path only.
pub fn canonical(a: &ureq::Agent, url: &str) -> Result<String, String> {
    let u = normalize(url);
    let host = host_of(&u).unwrap_or_default();
    if host == "soundcloud.com" { return Ok(strip_query(&u)); }
    let r = a.get(&u).call().map_err(|e| format!("short link: {e}"))?;
    let fin = strip_query(r.get_url());
    if host_of(&fin).map(|h| h == "soundcloud.com").unwrap_or(false) {
        Ok(fin)
    } else {
        Err(format!("short link resolved to a non-SoundCloud page: {fin}"))
    }
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

/// finds `client_id` followed by a quote/colon/equals and an alphanumeric token (tolerates `client_id:"x"`, `client_id: "x"`, `client_id="x"`)
fn find_cid(js: &str) -> Option<String> {
    let b = js.as_bytes();
    let mut from = 0;
    while let Some(i) = js[from..].find("client_id") {
        let mut p = from + i + "client_id".len();
        while p < b.len() && matches!(b[p], b'"' | b'\'' | b':' | b' ' | b'=') { p += 1; }
        let id: String = js[p..].chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
        if (18..=64).contains(&id.len()) { return Some(id); }
        from = p;
    }
    None
}

/// script URLs referenced by the homepage (src="...sndcdn.com...js"), with a fallback scan of absolute URLs
fn script_urls(html: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find("src=\"") {
        let after = &rest[i + 5..];
        let end = after.find('"').unwrap_or(after.len());
        let u = &after[..end];
        if u.contains("sndcdn.com") && u.contains(".js") && !out.iter().any(|x| x == u) { out.push(u.to_string()); }
        rest = &after[end..];
    }
    if out.is_empty() {
        let mut r = html;
        while let Some(i) = r.find("https://") {
            let t = &r[i..];
            let end = t.find(|c: char| c == '"' || c == '\'' || c.is_whitespace()).unwrap_or(t.len());
            let u = &t[..end];
            if u.contains("sndcdn.com") && u.contains(".js") && !out.iter().any(|x| x == u) { out.push(u.to_string()); }
            r = &t[end..];
        }
    }
    out
}

fn fetch_client_id(a: &ureq::Agent) -> Result<String, String> {
    let html = get_text(a, "https://soundcloud.com/")?;
    let scripts = script_urls(&html);
    if scripts.is_empty() { return Err("soundcloud: no script links on the homepage (site layout changed?)".into()); }
    for s in scripts.iter().rev() {
        if let Ok(js) = get_text(a, s) { if let Some(id) = find_cid(&js) { return Ok(id); } }
    }
    Err(format!("soundcloud: client_id not found in {} scripts", scripts.len()))
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

/// GET an api-v2 URL with client_id appended; refreshes the client_id once on 401/403
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
    let url = canonical(&a, url)?;
    let v = api(&a, &format!("https://api-v2.soundcloud.com/resolve?url={}", enc(&url)))?;
    match v["kind"].as_str() {
        Some("track") => parse_track(&v).map(Resolved::Track).ok_or_else(|| "soundcloud: track unavailable".to_string()),
        Some("playlist") | Some("system-playlist") => {
            let name = v["title"].as_str().unwrap_or("Playlist").to_string();
            let items = v["tracks"].as_array().cloned().unwrap_or_default();
            // playlist entries past the first few are stubs ({id, kind}); fetch them in chunks, keep order
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

/// pick an MP3 transcoding: progressive first, then HLS. Returns (transcoding, is_hls)
fn choose(v: &Value) -> Result<(Value, bool), String> {
    let tr = v["media"]["transcodings"].as_array()
        .ok_or_else(|| "soundcloud: no transcodings (track not streamable?)".to_string())?;
    let mpeg = |t: &Value| t["format"]["mime_type"].as_str().unwrap_or("").contains("mpeg");
    let proto = |t: &Value| t["format"]["protocol"].as_str().unwrap_or("").to_string();
    if let Some(t) = tr.iter().find(|t| proto(t) == "progressive" && mpeg(t)) { return Ok((t.clone(), false)); }
    if let Some(t) = tr.iter().find(|t| proto(t) == "hls" && mpeg(t)) { return Ok((t.clone(), true)); }
    let avail: Vec<String> = tr.iter()
        .map(|t| format!("{}/{}", proto(t), t["format"]["mime_type"].as_str().unwrap_or("?")))
        .collect();
    Err(format!("soundcloud: no MP3 stream (available: {})", avail.join(", ")))
}

/// Resolve a permalink to a playable media URL (done on the producer thread right before playback).
pub fn stream_for_permalink(a: &ureq::Agent, permalink: &str) -> Result<Stream, String> {
    let canon = canonical(a, permalink)?;
    let v = api(a, &format!("https://api-v2.soundcloud.com/resolve?url={}", enc(&canon)))?;
    let (t, is_hls) = choose(&v)?;
    let tu = t["url"].as_str().ok_or("soundcloud: transcoding without url")?;
    let auth = v["track_authorization"].as_str().unwrap_or("");
    let j = api(a, &format!("{tu}?track_authorization={}", enc(auth)))?;
    let u = j["url"].as_str().ok_or("soundcloud: no stream url")?.to_string();
    Ok(if is_hls { Stream::Hls { url: u, ext: "mp3" } } else { Stream::Progressive(u) })
}

/// Step-by-step check of one link. Never returns secrets (client_id is reported by length only).
pub fn diagnose(url: &str) -> Value {
    let a = agent();
    let mut log: Vec<Value> = Vec::new();
    let fail = |log: Vec<Value>, step: &str, e: String| json!({"ok": false, "failedAt": step, "error": e, "steps": log});

    let canon = match canonical(&a, url) {
        Ok(c) => { log.push(json!({"step": "canonical", "url": c})); c }
        Err(e) => return fail(log, "canonical", e),
    };
    match client_id(&a, false) {
        Ok(id) => log.push(json!({"step": "client_id", "length": id.len()})),
        Err(e) => return fail(log, "client_id", e),
    }
    let v = match api(&a, &format!("https://api-v2.soundcloud.com/resolve?url={}", enc(&canon))) {
        Ok(v) => v,
        Err(e) => return fail(log, "resolve", e),
    };
    let trs: Vec<Value> = v["media"]["transcodings"].as_array().cloned().unwrap_or_default();
    let kinds: Vec<Value> = trs.iter().map(|t| json!({"protocol": t["format"]["protocol"], "mime": t["format"]["mime_type"]})).collect();
    log.push(json!({"step": "resolve", "kind": v["kind"], "title": v["title"], "policy": v["policy"], "transcodings": kinds}));

    match choose(&v) {
        Ok((t, hls)) => {
            let proto = if hls { "hls" } else { "progressive" };
            log.push(json!({"step": "choose", "protocol": proto, "mime": t["format"]["mime_type"]}));
        }
        Err(e) => return fail(log, "choose", e),
    }
    match stream_for_permalink(&a, &canon) {
        Ok(Stream::Progressive(_)) => log.push(json!({"step": "stream", "type": "progressive", "ok": true})),
        Ok(Stream::Hls { .. }) => log.push(json!({"step": "stream", "type": "hls", "ok": true})),
        Err(e) => return fail(log, "stream", e),
    }
    json!({"ok": true, "steps": log})
}
