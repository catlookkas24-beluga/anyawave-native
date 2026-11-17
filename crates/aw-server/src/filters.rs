//! Lavalink-v4 `filters` JSON  ->  DSP chain keys, plus AnyaWave extensions and presets.
//! Standard Lavalink filters: volume, equalizer, karaoke, timescale, tremolo, vibrato, distortion, rotation, channelMix, lowPass.
//! Extensions (read from `filters.<name>` or `filters.pluginFilters.<name>` so ordinary Lavalink clients can send them):
//!   preamp(dB) parametricEq[] stereo echo reverb compressor limiter preset
use aw_engine::dsp::FilterChain;
use serde_json::{json, Value};

fn num(v: &Value) -> Option<f64> { v.as_f64() }

/// look in filters.<name>, then filters.pluginFilters.<name>
fn ext<'a>(f: &'a Value, name: &str) -> Option<&'a Value> {
    f.get(name).or_else(|| f.get("pluginFilters").and_then(|p| p.get(name)))
}

fn type_id(s: &str) -> i32 {
    match s { "peaking" | "peak" | "bell" => 0, "lowshelf" => 1, "highshelf" => 2, "lowpass" => 3, "highpass" => 4,
              "bandpass" => 5, "notch" => 6, "allpass" => 7, _ => 0 }
}
fn mode_id(s: &str) -> i32 { match s { "left" => 1, "right" => 2, "mid" => 3, "side" => 4, _ => 0 } }

type Kv = Vec<(String, f64)>;

fn put(kv: &mut Kv, key: &str, v: f64) { kv.push((key.to_string(), v)); }

fn band(kv: &mut Kv, i: usize, ty: &str, f: f64, g: f64, q: f64) {
    if i >= 32 { return; }
    put(kv, &format!("eq.{i}.type"), type_id(ty) as f64);
    put(kv, &format!("eq.{i}.freq"), f);
    put(kv, &format!("eq.{i}.gain"), g);
    put(kv, &format!("eq.{i}.q"), q);
}

/// returns number of parametric bands used
fn preset(kv: &mut Kv, name: &str) -> usize {
    let mut n = 0usize;
    macro_rules! b { ($t:expr, $f:expr, $g:expr, $q:expr) => {{ band(kv, n, $t, $f, $g, $q); n += 1; }}; }
    match name {
        "flat" => {}
        "bassboost" => { b!("lowshelf", 100.0, 6.0, 0.7); }
        "bassboost_extreme" => { b!("lowshelf", 100.0, 10.0, 0.7); b!("peaking", 50.0, 4.0, 1.0); }
        "treble" => { b!("highshelf", 6000.0, 5.0, 0.7); }
        "vocal" => { b!("peaking", 250.0, -2.0, 1.0); b!("peaking", 3000.0, 3.0, 1.0); b!("highshelf", 8000.0, 1.0, 0.7); }
        "loudness" => { b!("lowshelf", 80.0, 6.0, 0.7); b!("peaking", 3000.0, -2.0, 0.8); b!("highshelf", 9000.0, 4.0, 0.7); }
        "clarity" => { b!("peaking", 300.0, -2.0, 1.0); b!("peaking", 2500.0, 2.0, 1.0); b!("highshelf", 10000.0, 3.0, 0.7); }
        "warm" => { b!("lowshelf", 200.0, 3.0, 0.7); b!("highshelf", 8000.0, -2.0, 0.7); }
        "telephone" => { b!("highpass", 300.0, 0.0, 0.7); b!("lowpass", 3400.0, 0.0, 0.7); b!("peaking", 1500.0, 4.0, 1.0); }
        "nightcore" => { put(kv, "timescale.speed", 1.2); put(kv, "timescale.pitch", 1.2); }
        "slowed" => { put(kv, "timescale.speed", 0.85); put(kv, "timescale.pitch", 0.85); put(kv, "reverb.wet", 0.15); put(kv, "reverb.room", 0.7); }
        "vaporwave" => { put(kv, "timescale.speed", 0.8); put(kv, "timescale.pitch", 0.8); put(kv, "reverb.wet", 0.22); put(kv, "reverb.room", 0.8); b!("highshelf", 6000.0, -4.0, 0.7); }
        "chipmunk" => { put(kv, "timescale.pitch", 1.5); }
        "deep" => { put(kv, "timescale.pitch", 0.75); }
        "8d" => { put(kv, "rotation.hz", 0.2); put(kv, "reverb.wet", 0.12); }
        "karaoke" => { put(kv, "karaoke.on", 1.0); }
        _ => {}
    }
    n
}

pub fn presets() -> &'static [&'static str] {
    &["flat", "bassboost", "bassboost_extreme", "treble", "vocal", "loudness", "clarity", "warm", "telephone",
      "nightcore", "slowed", "vaporwave", "chipmunk", "deep", "8d", "karaoke"]
}

/// Apply a full filter set (replaces whatever was active). `volume_pct` is the Lavalink player volume (100 = unity).
pub fn apply(chain: &mut FilterChain, f: &Value, volume_pct: u32) {
    let mut kv: Kv = Vec::new();
    let mut next_band = 0usize;

    // presets first, so explicit settings below override them
    if let Some(p) = ext(f, "preset") {
        let names: Vec<&str> = match p { Value::String(s) => vec![s.as_str()], Value::Array(a) => a.iter().filter_map(|x| x.as_str()).collect(), _ => vec![] };
        for n in names { next_band += preset(&mut kv, n); }
    }

    // ---- standard Lavalink filters
    let fvol = f.get("volume").and_then(num).unwrap_or(1.0);
    put(&mut kv, "gain.volume", volume_pct as f64 / 100.0 * fvol);

    if let Some(arr) = f.get("equalizer").and_then(|v| v.as_array()) {
        for b in arr {
            if let (Some(i), Some(g)) = (b.get("band").and_then(|v| v.as_u64()), b.get("gain").and_then(num)) { put(&mut kv, &format!("geq.{i}"), g); }
        }
    }
    if let Some(k) = f.get("karaoke").filter(|v| v.is_object()) {
        put(&mut kv, "karaoke.on", 1.0);
        for (jk, ck) in [("level", "level"), ("monoLevel", "mono"), ("filterBand", "band"), ("filterWidth", "width")] {
            if let Some(v) = k.get(jk).and_then(num) { put(&mut kv, &format!("karaoke.{ck}"), v); }
        }
    }
    if let Some(t) = f.get("timescale").filter(|v| v.is_object()) {
        for k in ["speed", "pitch", "rate"] { if let Some(v) = t.get(k).and_then(num) { put(&mut kv, &format!("timescale.{k}"), v); } }
    }
    if let Some(t) = f.get("tremolo").filter(|v| v.is_object()) {
        if let Some(v) = t.get("frequency").and_then(num) { put(&mut kv, "tremolo.freq", v); }
        if let Some(v) = t.get("depth").and_then(num) { put(&mut kv, "tremolo.depth", v); }
    }
    if let Some(t) = f.get("vibrato").filter(|v| v.is_object()) {
        if let Some(v) = t.get("frequency").and_then(num) { put(&mut kv, "vibrato.freq", v); }
        if let Some(v) = t.get("depth").and_then(num) { put(&mut kv, "vibrato.depth", v); }
    }
    if let Some(d) = f.get("distortion").filter(|v| v.is_object()) {
        put(&mut kv, "distortion.on", 1.0);
        for k in ["sinOffset", "sinScale", "cosOffset", "cosScale", "tanOffset", "tanScale", "offset", "scale"] {
            if let Some(v) = d.get(k).and_then(num) { put(&mut kv, &format!("distortion.{}", k.to_lowercase()), v); }
        }
    }
    if let Some(v) = f.get("rotation").and_then(|r| r.get("rotationHz")).and_then(num) { put(&mut kv, "rotation.hz", v); }
    if let Some(m) = f.get("channelMix").filter(|v| v.is_object()) {
        for (jk, ck) in [("leftToLeft", "ll"), ("leftToRight", "lr"), ("rightToLeft", "rl"), ("rightToRight", "rr")] {
            if let Some(v) = m.get(jk).and_then(num) { put(&mut kv, &format!("mix.{ck}"), v); }
        }
    }
    if let Some(v) = f.get("lowPass").and_then(|l| l.get("smoothing")).and_then(num) { put(&mut kv, "lowpass.smoothing", v); }

    // ---- AnyaWave extensions
    if let Some(v) = ext(f, "preamp").and_then(num) { put(&mut kv, "gain.preamp", v); }
    if let Some(arr) = ext(f, "parametricEq").and_then(|v| v.as_array()) {
        for b in arr {
            if next_band >= 32 { break; }
            if b.get("enabled").and_then(|v| v.as_bool()) == Some(false) { continue; }
            let i = next_band; next_band += 1;
            let ty = b.get("type").and_then(|v| v.as_str()).unwrap_or("peaking");
            band(&mut kv, i, ty, b.get("frequency").and_then(num).unwrap_or(1000.0), b.get("gain").and_then(num).unwrap_or(0.0), b.get("q").and_then(num).unwrap_or(1.0));
            if let Some(m) = b.get("mode").and_then(|v| v.as_str()) { put(&mut kv, &format!("eq.{i}.mode"), mode_id(m) as f64); }
        }
    }
    if let Some(s) = ext(f, "stereo").filter(|v| v.is_object()) {
        for (jk, ck) in [("width", "width"), ("balance", "balance")] { if let Some(v) = s.get(jk).and_then(num) { put(&mut kv, &format!("mix.{ck}"), v); } }
        for (jk, ck) in [("mono", "mono"), ("swap", "swap"), ("invertLeft", "invl"), ("invertRight", "invr")] {
            if let Some(v) = s.get(jk).and_then(|v| v.as_bool()) { put(&mut kv, &format!("mix.{ck}"), if v { 1.0 } else { 0.0 }); }
        }
    }
    if let Some(e) = ext(f, "echo").filter(|v| v.is_object()) {
        for (jk, ck) in [("delay", "time"), ("feedback", "feedback"), ("mix", "mix")] { if let Some(v) = e.get(jk).and_then(num) { put(&mut kv, &format!("echo.{ck}"), v); } }
    }
    if let Some(r) = ext(f, "reverb").filter(|v| v.is_object()) {
        for (jk, ck) in [("room", "room"), ("damping", "damp"), ("wet", "wet"), ("dry", "dry"), ("width", "width")] {
            if let Some(v) = r.get(jk).and_then(num) { put(&mut kv, &format!("reverb.{ck}"), v); }
        }
    }
    if let Some(c) = ext(f, "compressor").filter(|v| v.is_object()) {
        put(&mut kv, "comp.on", 1.0);
        for k in ["threshold", "ratio", "attack", "release", "makeup", "knee"] { if let Some(v) = c.get(k).and_then(num) { put(&mut kv, &format!("comp.{k}"), v); } }
    }
    if let Some(l) = ext(f, "limiter").filter(|v| v.is_object()) {
        if let Some(v) = l.get("enabled").and_then(|v| v.as_bool()) { put(&mut kv, "limiter.on", if v { 1.0 } else { 0.0 }); }
        if let Some(v) = l.get("ceiling").and_then(num) { put(&mut kv, "limiter.ceiling", v); }
        if let Some(v) = l.get("release").and_then(num) { put(&mut kv, "limiter.release", v); }
    }

    chain.reset_params();
    for (k, v) in kv { chain.set(&k, v); }
    chain.commit();
}

/// Human-readable description of everything the server understands (GET /v4/filters).
pub fn schema() -> Value {
    json!({
      "standard": {
        "volume": "float, multiplier (1.0 = unity, up to 5.0; a limiter protects against clipping)",
        "equalizer": "[{band 0..14, gain -0.25..1.0}] 15-band graphic EQ (25 Hz .. 16 kHz); gain 0.25 = +6 dB, -0.25 = muted",
        "karaoke": "{level, monoLevel, filterBand(Hz), filterWidth(Hz)} centre-channel vocal reduction (approximation)",
        "timescale": "{speed, pitch, rate} 0.1..8 (WSOLA time-stretch + resampler)",
        "tremolo": "{frequency Hz, depth 0..1}", "vibrato": "{frequency Hz, depth 0..1}",
        "distortion": "{sinOffset, sinScale, cosOffset, cosScale, tanOffset, tanScale, offset, scale} (approximation)",
        "rotation": "{rotationHz} 8D audio", "channelMix": "{leftToLeft, leftToRight, rightToLeft, rightToRight}",
        "lowPass": "{smoothing > 1}"
      },
      "extensions": {
        "note": "put these directly in `filters` or under `filters.pluginFilters`",
        "preamp": "dB, applied before everything",
        "parametricEq": "up to 32 bands: [{type: peaking|lowshelf|highshelf|lowpass|highpass|bandpass|notch|allpass, frequency Hz (5..~24k), gain dB, q 0.05.., mode: stereo|left|right|mid|side, enabled}]",
        "stereo": "{width 0..4, balance -1..1, mono, swap, invertLeft, invertRight}",
        "echo": "{delay ms 1..2000, feedback 0..0.95, mix 0..1}",
        "reverb": "{room 0..1, damping 0..1, wet 0..1, dry 0..1, width 0..1}",
        "compressor": "{threshold dB, ratio, attack ms, release ms, makeup dB, knee dB}",
        "limiter": "{enabled, ceiling dB (<= 0), release ms}  (automatic when any filter is active)",
        "preset": format!("name or [names], expanded before your explicit values: {}", presets().join(", "))
      },
      "chainOrder": ["timescale", "preamp", "channelMix/stereo", "EQ (graphic + parametric)", "karaoke", "lowPass", "distortion", "vibrato", "tremolo", "rotation", "echo", "reverb", "compressor", "volume", "limiter"]
    })
}
