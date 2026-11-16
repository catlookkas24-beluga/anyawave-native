# AnyaWave Native (Rust + C++)

Lavalink-v4-compatible audio server. **Status: design + uncompiled skeleton.**
(Written in a sandbox with no cargo / no network: nothing here has been built or benchmarked.)

## Goals -> how each is met
| Goal | Mechanism |
|---|---|
| Many rooms | Sharded tick threads (<=4), NOT one thread per guild. Players are plain structs in a shard. |
| Low resource | No GC/JVM. mimalloc. Opus **passthrough** when no filter + Opus source. Reused buffers on hot path. sendmmsg batching. |
| Clear sound | f32 pipeline, soxr/rubato resample, libopus 96-128 kbps VBR complexity 5-6, clip guard. |
| No stutter | Producer/consumer split: decode+encode run ahead into an SPSC ring (~300 ms); tick thread only pops/encrypts/sends. Absolute-deadline 20 ms clock (no drift), SCHED_FIFO if allowed. Underrun => RTP ts/seq still advance. |
| DAVE | Voice gateway v8 + `max_dave_protocol_version`, MLS ops 21-31, Discord libdave (C++) behind a tiny C ABI. Passthrough until first epoch is ready; never blocks audio. |

## Layout
- `crates/aw-engine`  scheduler, ring, producer pipeline, FFI to `cpp/dsp.cpp`
- `crates/aw-voice`   gateway v8, DAVE state machine, RTP, AEAD transport crypto, UDP
- `crates/aw-server`  Lavalink v4 REST + WebSocket (axum)
- `cpp/dsp.cpp`       filters (volume, 15-band EQ, tremolo done; timescale/karaoke/rotation TODO)
- `cpp/dave_bridge.*` C ABI over libdave. **Only partly implemented; libdave symbol names marked VERIFY.**

## Per-tick data path (every 20 ms per player)
ring.pop(opus) -> libdave encrypt (E2EE) -> RTP header -> AES-256-GCM / XChaCha20 (rtpsize) -> batch -> sendmmsg

## Things you must verify before trusting it
1. libdave API names/signatures: pin a release tag as `third_party/libdave` and fix `dave_bridge.cpp`.
2. Binary voice-frame layout and nonce counter endianness vs current Discord voice docs.
3. Lavaplayer track-blob (base64) encode/decode for client compatibility.
4. Sources: YouTube needs a cipher/poToken solution (sidecar or plugin); HTTP/local/SoundCloud are easy.

## Roadmap
1. M1: one guild, HTTP source, Opus encode, transport crypto, no DAVE -> hear audio.
2. M2: DAVE handshake against a real voice channel.
3. M3: shards + sendmmsg + ring tuning; load test 500+ players, track `frameStats.deficit`.
4. M4: filters (timescale first), resume, plugin-style SourceManager.
5. M5: metrics, fuzz the binary parsers.
