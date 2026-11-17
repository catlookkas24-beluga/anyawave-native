// AnyaWave DSP chain. Interleaved stereo float, 48 kHz typical. All filter maths in double.
// Control plane:  aw_dsp_reset_params / aw_dsp_set("key", v) ... / aw_dsp_commit      (key list: see Dsp::set)
// Data plane:     aw_dsp_timescale (variable-length pre-stage)  ->  aw_dsp_process (fixed frames, in place)
// No allocation inside process(); delay lines are allocated in commit() (control thread, under the Rust mutex).
#include <algorithm>
#include <cmath>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>
#if defined(__SSE__)
#include <xmmintrin.h>
#endif

namespace {

constexpr double kPi = 3.14159265358979323846;
inline double db2lin(double db) { return std::pow(10.0, db / 20.0); }
inline double lin2db(double x) { return 20.0 * std::log10(x < 1e-9 ? 1e-9 : x); }
template <class T> inline T clampv(T v, T lo, T hi) { return v < lo ? lo : (v > hi ? hi : v); }
inline void flush_denormals() {
#if defined(__SSE__)
    _mm_setcsr(_mm_getcsr() | 0x8040);   // FTZ | DAZ: reverb/echo tails must not stall the CPU
#endif
}

// ----------------------------------------------------------------------------- biquad (RBJ cookbook)
enum { BQ_PEAK = 0, BQ_LOWSHELF, BQ_HIGHSHELF, BQ_LOWPASS, BQ_HIGHPASS, BQ_BANDPASS, BQ_NOTCH, BQ_ALLPASS };

struct Biquad {
    double b0 = 1, b1 = 0, b2 = 0, a1 = 0, a2 = 0;
    double z1[2] = {0, 0}, z2[2] = {0, 0};
    void design(int type, double sr, double f, double gainDb, double q) {
        f = clampv(f, 5.0, sr * 0.49);
        q = std::max(q, 0.05);
        double w0 = 2 * kPi * f / sr, cw = std::cos(w0), sw = std::sin(w0), al = sw / (2 * q);
        double A = std::pow(10.0, gainDb / 40.0), sA = std::sqrt(A);
        double a0 = 1, B0 = 1, B1 = 0, B2 = 0, A1 = 0, A2 = 0;
        switch (type) {
        case BQ_PEAK:
            B0 = 1 + al * A; B1 = -2 * cw; B2 = 1 - al * A; a0 = 1 + al / A; A1 = -2 * cw; A2 = 1 - al / A; break;
        case BQ_LOWSHELF:
            B0 = A * ((A + 1) - (A - 1) * cw + 2 * sA * al); B1 = 2 * A * ((A - 1) - (A + 1) * cw);
            B2 = A * ((A + 1) - (A - 1) * cw - 2 * sA * al);
            a0 = (A + 1) + (A - 1) * cw + 2 * sA * al; A1 = -2 * ((A - 1) + (A + 1) * cw); A2 = (A + 1) + (A - 1) * cw - 2 * sA * al; break;
        case BQ_HIGHSHELF:
            B0 = A * ((A + 1) + (A - 1) * cw + 2 * sA * al); B1 = -2 * A * ((A - 1) + (A + 1) * cw);
            B2 = A * ((A + 1) + (A - 1) * cw - 2 * sA * al);
            a0 = (A + 1) - (A - 1) * cw + 2 * sA * al; A1 = 2 * ((A - 1) - (A + 1) * cw); A2 = (A + 1) - (A - 1) * cw - 2 * sA * al; break;
        case BQ_LOWPASS:
            B0 = (1 - cw) / 2; B1 = 1 - cw; B2 = (1 - cw) / 2; a0 = 1 + al; A1 = -2 * cw; A2 = 1 - al; break;
        case BQ_HIGHPASS:
            B0 = (1 + cw) / 2; B1 = -(1 + cw); B2 = (1 + cw) / 2; a0 = 1 + al; A1 = -2 * cw; A2 = 1 - al; break;
        case BQ_BANDPASS:
            B0 = al; B1 = 0; B2 = -al; a0 = 1 + al; A1 = -2 * cw; A2 = 1 - al; break;
        case BQ_NOTCH:
            B0 = 1; B1 = -2 * cw; B2 = 1; a0 = 1 + al; A1 = -2 * cw; A2 = 1 - al; break;
        default: // allpass
            B0 = 1 - al; B1 = -2 * cw; B2 = 1 + al; a0 = 1 + al; A1 = -2 * cw; A2 = 1 - al; break;
        }
        b0 = B0 / a0; b1 = B1 / a0; b2 = B2 / a0; a1 = A1 / a0; a2 = A2 / a0;
    }
    inline double run(int ch, double x) {      // transposed direct form II
        double y = b0 * x + z1[ch];
        z1[ch] = b1 * x - a1 * y + z2[ch];
        z2[ch] = b2 * x - a2 * y;
        return y;
    }
    void clear() { z1[0] = z1[1] = z2[0] = z2[1] = 0; }
};

// Lavalink graphic-EQ centre frequencies (15 bands)
constexpr double kGeqFreq[15] = {25, 40, 63, 100, 160, 250, 400, 630, 1000, 1600, 2500, 4000, 6300, 10000, 16000};
constexpr double kGeqQ = 2.14;          // ~2/3 octave
// Lavalink gain (-0.25 .. 1.0): -0.25 = muted, 0.25 = x2 (+6 dB)  =>  amplitude = 1 + 4g
inline double geqToDb(double g) { double a = 1.0 + 4.0 * g; return a <= 0.001 ? -60.0 : 20.0 * std::log10(a); }

constexpr int kEqBands = 32;
struct EqSpec { bool on = false; int type = BQ_PEAK; double f = 1000, gain = 0, q = 1.0; int mode = 0; };

// ----------------------------------------------------------------------------- parameters
struct Params {
    double preampDb = 0, volume = 1.0;
    double mix[4] = {1, 0, 0, 1};                 // ll lr rl rr  (Lavalink channelMix: L' = l*ll + r*rl ; R' = l*lr + r*rr)
    bool mono = false, swap = false, invL = false, invR = false;
    double balance = 0, width = 1.0;
    EqSpec eq[kEqBands];
    double geq[15] = {0};
    bool kOn = false; double kLevel = 1.0, kMono = 1.0, kBand = 220.0, kWidth = 100.0;
    double lpSmooth = 0;                           // >1 enables (Lavalink lowPass.smoothing)
    bool dOn = false; double dSinO = 0, dSinS = 1, dCosO = 0, dCosS = 1, dTanO = 0, dTanS = 1, dOff = 0, dScale = 1;
    double vibF = 0, vibD = 0, tremF = 0, tremD = 0, rotHz = 0;
    double echoMs = 0, echoFb = 0.4, echoMix = 0;
    double rvRoom = 0.5, rvDamp = 0.5, rvWet = 0, rvDry = 1.0, rvWidth = 1.0;
    bool cOn = false; double cThr = -18, cRatio = 3, cAtk = 10, cRel = 120, cMake = 0, cKnee = 6;
    int limOn = -1;                                // -1 auto (on whenever any filter is active), 0 off, 1 on
    double limCeil = -0.3, limRel = 80;
    double tsSpeed = 1, tsPitch = 1, tsRate = 1;
};

// ----------------------------------------------------------------------------- reverb (Freeverb topology)
struct Comb {
    std::vector<float> buf; size_t idx = 0; float store = 0;
    void init(size_t n) { buf.assign(std::max<size_t>(n, 1), 0.f); idx = 0; store = 0; }
    inline float process(float in, float fb, float d1, float d2) {
        float out = buf[idx]; store = out * d2 + store * d1; buf[idx] = in + store * fb;
        if (++idx >= buf.size()) idx = 0;
        return out;
    }
};
struct Allpass {
    std::vector<float> buf; size_t idx = 0;
    void init(size_t n) { buf.assign(std::max<size_t>(n, 1), 0.f); idx = 0; }
    inline float process(float in) {
        float b = buf[idx]; float out = -in + b; buf[idx] = in + b * 0.5f;
        if (++idx >= buf.size()) idx = 0;
        return out;
    }
};
struct Reverb {
    bool ready = false; Comb cl[8], cr[8]; Allpass al[4], ar[4];
    void init(double sr) {
        const int ct[8] = {1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617}, at[4] = {556, 441, 341, 225};
        double k = sr / 44100.0;
        for (int i = 0; i < 8; i++) { cl[i].init((size_t)std::lround(ct[i] * k)); cr[i].init((size_t)std::lround((ct[i] + 23) * k)); }
        for (int i = 0; i < 4; i++) { al[i].init((size_t)std::lround(at[i] * k)); ar[i].init((size_t)std::lround((at[i] + 23) * k)); }
        ready = true;
    }
    void clear() { if (!ready) return; for (auto& c : cl) { std::fill(c.buf.begin(), c.buf.end(), 0.f); c.store = 0; }
                   for (auto& c : cr) { std::fill(c.buf.begin(), c.buf.end(), 0.f); c.store = 0; }
                   for (auto& a : al) std::fill(a.buf.begin(), a.buf.end(), 0.f);
                   for (auto& a : ar) std::fill(a.buf.begin(), a.buf.end(), 0.f); }
};

// ----------------------------------------------------------------------------- time-scale / pitch (WSOLA + resampler)
// speed = tempo (pitch kept), pitch = pitch (tempo kept), rate = both.   Ts = speed/pitch,  r = pitch*rate
struct TimeScale {
    int ovl = 480, hop = 1440, seek = 720;   // 10 ms overlap, 30 ms hop, +-7.5 ms search (at 48 kHz)
    double Ts = 1.0, r = 1.0;
    std::vector<float> in;                   // interleaved stereo, absolute frame index of in[0] = base
    long long base = 0; double pos = 0; long long cont = 0; bool first = true;
    std::vector<float> w;                    // WSOLA output waiting for the resampler
    double rpos = 0;

    void configure(double sr) {
        double k = sr / 48000.0;
        ovl = (int)std::lround(480 * k); hop = (int)std::lround(1440 * k); seek = (int)std::lround(720 * k);
    }
    void reset() { in.clear(); w.clear(); base = 0; pos = 0; cont = 0; first = true; rpos = 0; }
    bool active() const { return std::fabs(Ts - 1.0) > 1e-3 || std::fabs(r - 1.0) > 1e-3; }

    inline float at(long long frame, int ch) const { return in[(size_t)(frame - base) * 2 + ch]; }

    void wsola() {
        const double Ha = hop * Ts;
        const int D = seek / 2;
        for (;;) {
            long long avail = base + (long long)(in.size() / 2);
            if (first) {
                if (avail < base + hop + ovl) return;
                for (int i = 0; i < hop; i++) { w.push_back(at(base + i, 0)); w.push_back(at(base + i, 1)); }
                cont = base + hop; pos = (double)base + Ha; first = false;
                continue;
            }
            long long need = std::max<long long>((long long)pos + D + hop + ovl + 2, cont + ovl + 2);
            if (avail < need) return;
            long long lo = std::max<long long>((long long)pos - D, base), hi = (long long)pos + D;
            // search the offset whose overlap best matches the natural continuation (normalised correlation on decimated mono)
            int n2 = ovl / 2;
            double bestScore = -1e300; long long bestStart = (long long)pos;
            for (long long s = lo; s <= hi; s++) {
                double c = 0, e = 1e-9;
                for (int j = 0; j < n2; j++) {
                    double t = at(cont + 2 * j, 0) + at(cont + 2 * j, 1);
                    double x = at(s + 2 * j, 0) + at(s + 2 * j, 1);
                    c += t * x; e += x * x;
                }
                double sc = c / std::sqrt(e);
                if (sc > bestScore) { bestScore = sc; bestStart = s; }
            }
            long long st = bestStart;
            for (int k = 0; k < ovl; k++) {
                double wt = 0.5 - 0.5 * std::cos(kPi * (double)k / (double)ovl);
                for (int ch = 0; ch < 2; ch++)
                    w.push_back((float)(at(cont + k, ch) * (1.0 - wt) + at(st + k, ch) * wt));
            }
            for (int k = ovl; k < hop; k++) { w.push_back(at(st + k, 0)); w.push_back(at(st + k, 1)); }
            cont = st + hop; pos += Ha;
            // drop consumed input
            long long keep = std::min<long long>((long long)pos - D, cont) - 2;
            if (keep > base + 4096) { in.erase(in.begin(), in.begin() + (size_t)(keep - base) * 2); base = keep; }
        }
    }

    // Catmull-Rom resample w -> out at step r
    void resample(std::vector<float>& out) {
        size_t nf = w.size() / 2;
        if (std::fabs(r - 1.0) <= 1e-3) { out.insert(out.end(), w.begin(), w.end()); w.clear(); rpos = 0; return; }
        while ((size_t)rpos + 3 < nf) {
            size_t i = (size_t)rpos; double t = rpos - (double)i;
            for (int ch = 0; ch < 2; ch++) {
                double p0 = i == 0 ? w[ch] : w[(i - 1) * 2 + ch], p1 = w[i * 2 + ch], p2 = w[(i + 1) * 2 + ch], p3 = w[(i + 2) * 2 + ch];
                double a = -0.5 * p0 + 1.5 * p1 - 1.5 * p2 + 0.5 * p3, b = p0 - 2.5 * p1 + 2 * p2 - 0.5 * p3, c = -0.5 * p0 + 0.5 * p2;
                out.push_back((float)(((a * t + b) * t + c) * t + p1));
            }
            rpos += r;
        }
        size_t drop = (size_t)rpos; if (drop > 2) drop -= 2; else drop = 0;
        if (drop > 0) { w.erase(w.begin(), w.begin() + drop * 2); rpos -= (double)drop; }
    }

    void process(const float* x, int frames, std::vector<float>& out) {
        in.insert(in.end(), x, x + (size_t)frames * 2);
        if (std::fabs(Ts - 1.0) > 1e-3) wsola();
        else { w.insert(w.end(), in.begin(), in.end()); in.clear(); base = 0; first = true; }
        resample(out);
    }
};

// ----------------------------------------------------------------------------- the chain
struct Dsp {
    double sr;
    Params p;
    // derived
    Biquad bq[15 + kEqBands];
    std::vector<int> act;
    Biquad kbp;
    bool kOn = false, lpOn = false, vibOn = false, tremOn = false, rotOn = false, echoOn = false, rvOn = false, cOn = false, mixOn = false, tsOn = false, active = false, limOn = false;
    double vTarget = 1.0, vCur = 1.0, preamp = 1.0;
    double limCeil = 0.97, limRelCoef = 0, limG = 1.0;
    double lpY[2] = {0, 0};
    double vibPh = 0, tremPh = 0, rotPh = 0;
    std::vector<float> vibBuf[2]; size_t vibIdx = 0;
    std::vector<float> echoBuf[2]; size_t echoIdx = 0, echoLen = 0;
    Reverb rv;
    double cEnv = 0;
    TimeScale ts;

    explicit Dsp(double s) : sr(s) { ts.configure(s); }

    static void split(const std::string& s, std::vector<std::string>& out) {
        size_t a = 0; for (;;) { size_t b = s.find('.', a); out.push_back(s.substr(a, b == std::string::npos ? b : b - a)); if (b == std::string::npos) break; a = b + 1; }
    }

    int set(const std::string& key, double v) {
        std::vector<std::string> t; split(key, t);
        if (t.size() < 2) return -1;
        const std::string &a = t[0], &b = t[1];
        if (a == "gain") { if (b == "preamp") p.preampDb = v; else if (b == "volume") p.volume = v; else return -1; }
        else if (a == "mix") {
            if (b == "ll") p.mix[0] = v; else if (b == "lr") p.mix[1] = v; else if (b == "rl") p.mix[2] = v; else if (b == "rr") p.mix[3] = v;
            else if (b == "mono") p.mono = v != 0; else if (b == "swap") p.swap = v != 0;
            else if (b == "invl") p.invL = v != 0; else if (b == "invr") p.invR = v != 0;
            else if (b == "balance") p.balance = clampv(v, -1.0, 1.0); else if (b == "width") p.width = clampv(v, 0.0, 4.0);
            else return -1;
        }
        else if (a == "eq" && t.size() == 3) {
            int i = std::atoi(b.c_str()); if (i < 0 || i >= kEqBands) return -1;
            EqSpec& e = p.eq[i]; const std::string& c = t[2];
            if (c == "on") { e.on = v != 0; return 0; }
            if (c == "type") e.type = clampv((int)v, 0, 7); else if (c == "freq") e.f = v; else if (c == "gain") e.gain = v;
            else if (c == "q") e.q = v; else if (c == "mode") e.mode = clampv((int)v, 0, 4); else return -1;
            e.on = true;
        }
        else if (a == "geq") { int i = std::atoi(b.c_str()); if (i < 0 || i >= 15) return -1; p.geq[i] = v; }
        else if (a == "karaoke") {
            if (b == "level") p.kLevel = v; else if (b == "mono") p.kMono = v; else if (b == "band") p.kBand = v; else if (b == "width") p.kWidth = v; else if (b != "on") return -1;
            p.kOn = (b == "on") ? v != 0 : true;
        }
        else if (a == "lowpass") { if (b == "smoothing") p.lpSmooth = v; else return -1; }
        else if (a == "distortion") {
            if (b == "sinoffset") p.dSinO = v; else if (b == "sinscale") p.dSinS = v; else if (b == "cosoffset") p.dCosO = v;
            else if (b == "cosscale") p.dCosS = v; else if (b == "tanoffset") p.dTanO = v; else if (b == "tanscale") p.dTanS = v;
            else if (b == "offset") p.dOff = v; else if (b == "scale") p.dScale = v; else if (b != "on") return -1;
            p.dOn = (b == "on") ? v != 0 : true;
        }
        else if (a == "vibrato") { if (b == "freq") p.vibF = v; else if (b == "depth") p.vibD = v; else return -1; }
        else if (a == "tremolo") { if (b == "freq") p.tremF = v; else if (b == "depth") p.tremD = v; else return -1; }
        else if (a == "rotation") { if (b == "hz") p.rotHz = v; else return -1; }
        else if (a == "echo") { if (b == "time") p.echoMs = v; else if (b == "feedback") p.echoFb = v; else if (b == "mix") p.echoMix = v; else return -1; }
        else if (a == "reverb") {
            if (b == "room") p.rvRoom = v; else if (b == "damp") p.rvDamp = v; else if (b == "wet") p.rvWet = v;
            else if (b == "dry") p.rvDry = v; else if (b == "width") p.rvWidth = v; else return -1;
        }
        else if (a == "comp") {
            if (b == "on") p.cOn = v != 0; else if (b == "threshold") { p.cThr = v; p.cOn = true; } else if (b == "ratio") p.cRatio = v;
            else if (b == "attack") p.cAtk = v; else if (b == "release") p.cRel = v; else if (b == "makeup") p.cMake = v; else if (b == "knee") p.cKnee = v; else return -1;
        }
        else if (a == "limiter") { if (b == "on") p.limOn = v != 0 ? 1 : 0; else if (b == "ceiling") p.limCeil = v; else if (b == "release") p.limRel = v; else return -1; }
        else if (a == "timescale") { if (b == "speed") p.tsSpeed = v; else if (b == "pitch") p.tsPitch = v; else if (b == "rate") p.tsRate = v; else return -1; }
        else return -1;
        return 0;
    }

    void commit() {
        preamp = db2lin(p.preampDb); vTarget = p.volume;
        mixOn = p.mix[0] != 1 || p.mix[1] != 0 || p.mix[2] != 0 || p.mix[3] != 1 || p.mono || p.swap || p.invL || p.invR || p.balance != 0 || p.width != 1.0;
        // EQ: 15 graphic + 32 parametric
        act.clear();
        for (int i = 0; i < 15; i++) {
            double db = geqToDb(p.geq[i]);
            if (p.geq[i] != 0) { bq[i].design(BQ_PEAK, sr, kGeqFreq[i], db, kGeqQ); act.push_back(i); }
        }
        for (int i = 0; i < kEqBands; i++) {
            const EqSpec& e = p.eq[i]; if (!e.on) continue;
            bool gainType = e.type == BQ_PEAK || e.type == BQ_LOWSHELF || e.type == BQ_HIGHSHELF;
            if (gainType && e.gain == 0) continue;
            bq[15 + i].design(e.type, sr, e.f, e.gain, e.q); act.push_back(15 + i);
        }
        kOn = p.kOn && p.kLevel != 0 && p.kMono != 0;
        if (kOn) kbp.design(BQ_BANDPASS, sr, p.kBand, 0, std::max(p.kBand / std::max(p.kWidth, 1.0), 0.1));
        lpOn = p.lpSmooth > 1.0;
        vibOn = p.vibF > 0 && p.vibD > 0; tremOn = p.tremF > 0 && p.tremD > 0; rotOn = p.rotHz != 0;
        if (vibOn && vibBuf[0].empty()) { size_t n = (size_t)(sr * 0.02) + 8; vibBuf[0].assign(n, 0.f); vibBuf[1].assign(n, 0.f); vibIdx = 0; }
        echoOn = p.echoMs > 0 && p.echoMix > 0;
        if (echoOn) {
            size_t n = (size_t)(clampv(p.echoMs, 1.0, 2000.0) * sr / 1000.0);
            if (echoBuf[0].size() != n) { echoBuf[0].assign(n, 0.f); echoBuf[1].assign(n, 0.f); echoIdx = 0; }
            echoLen = n;
        }
        rvOn = p.rvWet > 0;
        if (rvOn && !rv.ready) rv.init(sr);
        cOn = p.cOn && p.cRatio > 1.0;
        double sp = clampv(p.tsSpeed, 0.1, 8.0), pi = clampv(p.tsPitch, 0.1, 8.0), ra = clampv(p.tsRate, 0.1, 8.0);
        ts.Ts = clampv(sp / pi, 0.25, 4.0); ts.r = clampv(pi * ra, 0.25, 4.0);
        bool wasTs = tsOn; tsOn = ts.active(); if (wasTs && !tsOn) ts.reset();
        bool anyEffect = preamp != 1.0 || vTarget != 1.0 || mixOn || !act.empty() || kOn || lpOn || p.dOn || vibOn || tremOn || rotOn || echoOn || rvOn || cOn;
        active = anyEffect || tsOn;
        limOn = p.limOn == -1 ? anyEffect : p.limOn == 1;
        limCeil = db2lin(std::min(p.limCeil, 0.0));
        limRelCoef = std::exp(-1.0 / (std::max(p.limRel, 1.0) * 0.001 * sr));
    }

    void reset_state() {
        for (auto& b : bq) b.clear();
        kbp.clear(); lpY[0] = lpY[1] = 0; vibPh = tremPh = rotPh = 0;
        for (int c = 0; c < 2; c++) { std::fill(vibBuf[c].begin(), vibBuf[c].end(), 0.f); std::fill(echoBuf[c].begin(), echoBuf[c].end(), 0.f); }
        rv.clear(); cEnv = 0; limG = 1.0; ts.reset(); vCur = vTarget;
    }

    double tempo() const { return tsOn ? ts.Ts * ts.r : 1.0; }

    void process(float* pcm, uint32_t frames) {
        flush_denormals();
        if (!active) return;
        const bool eqOn = !act.empty();
        const double dt = 1.0 / sr;
        const double vibMax = 0.008 * sr;
        const double balL = std::min(1.0, 1.0 - p.balance), balR = std::min(1.0, 1.0 + p.balance);
        const double cAtk = std::exp(-1.0 / (std::max(p.cAtk, 0.1) * 0.001 * sr)), cRel = std::exp(-1.0 / (std::max(p.cRel, 1.0) * 0.001 * sr));
        const double cSlope = cOn ? (1.0 / p.cRatio - 1.0) : 0.0, cMake = db2lin(p.cMake);
        const float rvFb = (float)(p.rvRoom * 0.28 + 0.7), rvD1 = (float)(p.rvDamp * 0.4), rvD2 = 1.0f - rvD1;
        const double rvW1 = p.rvWet * 3.0 * (p.rvWidth / 2 + 0.5), rvW2 = p.rvWet * 3.0 * ((1 - p.rvWidth) / 2);
        const double vk = 1.0 - std::exp(-1.0 / (0.01 * sr));   // 10 ms volume smoothing

        for (uint32_t i = 0; i < frames; i++) {
            double l = pcm[2 * i], r = pcm[2 * i + 1];
            l *= preamp; r *= preamp;
            if (mixOn) {
                double nl = l * p.mix[0] + r * p.mix[2], nr = l * p.mix[1] + r * p.mix[3]; l = nl; r = nr;
                if (p.mono) { l = r = 0.5 * (l + r); }
                if (p.swap) std::swap(l, r);
                if (p.invL) l = -l;
                if (p.invR) r = -r;
                l *= balL; r *= balR;
                if (p.width != 1.0) { double m = 0.5 * (l + r), s = 0.5 * (l - r) * p.width; l = m + s; r = m - s; }
            }
            if (eqOn) {
                for (int idx : act) {
                    Biquad& f = bq[idx];
                    int mode = idx < 15 ? 0 : p.eq[idx - 15].mode;
                    switch (mode) {
                    case 0: l = f.run(0, l); r = f.run(1, r); break;
                    case 1: l = f.run(0, l); break;
                    case 2: r = f.run(1, r); break;
                    case 3: { double m = 0.5 * (l + r), s = 0.5 * (l - r); m = f.run(0, m); l = m + s; r = m - s; break; }
                    default: { double m = 0.5 * (l + r), s = 0.5 * (l - r); s = f.run(1, s); l = m + s; r = m - s; break; }
                    }
                }
            }
            if (kOn) { double v = kbp.run(0, 0.5 * (l + r)) * p.kLevel * p.kMono; l -= v; r -= v; }
            if (lpOn) { lpY[0] += (l - lpY[0]) / p.lpSmooth; lpY[1] += (r - lpY[1]) / p.lpSmooth; l = lpY[0]; r = lpY[1]; }
            if (p.dOn) {
                auto d = [&](double x) {
                    double tn = std::tan(x + p.dTanO); tn = clampv(tn, -8.0, 8.0);
                    return p.dOff + p.dScale * (p.dSinS * std::sin(x + p.dSinO) + p.dCosS * std::cos(x + p.dCosO) + p.dTanS * tn) * 0.3333;
                };
                l = d(l); r = d(r);
            }
            if (vibOn) {
                double lfo = 0.5 + 0.5 * std::sin(vibPh); vibPh += 2 * kPi * p.vibF * dt; if (vibPh > 2 * kPi) vibPh -= 2 * kPi;
                double dly = 1.0 + clampv(p.vibD, 0.0, 1.0) * vibMax * lfo;
                size_t n = vibBuf[0].size();
                vibBuf[0][vibIdx] = (float)l; vibBuf[1][vibIdx] = (float)r;
                double rp = (double)vibIdx - dly; while (rp < 0) rp += (double)n;
                size_t i0 = (size_t)rp % n, i1 = (i0 + 1) % n; double fr = rp - std::floor(rp);
                l = vibBuf[0][i0] * (1 - fr) + vibBuf[0][i1] * fr; r = vibBuf[1][i0] * (1 - fr) + vibBuf[1][i1] * fr;
                vibIdx = (vibIdx + 1) % n;
            }
            if (tremOn) {
                double g = 1.0 - clampv(p.tremD, 0.0, 1.0) * (0.5 + 0.5 * std::sin(tremPh)); tremPh += 2 * kPi * p.tremF * dt; if (tremPh > 2 * kPi) tremPh -= 2 * kPi;
                l *= g; r *= g;
            }
            if (rotOn) {
                double ph = rotPh; rotPh += 2 * kPi * p.rotHz * dt; if (rotPh > 2 * kPi) rotPh -= 2 * kPi; else if (rotPh < -2 * kPi) rotPh += 2 * kPi;
                double a = (std::sin(ph) + 1.0) * kPi / 4.0;
                l *= std::cos(a) * 1.41421356; r *= std::sin(a) * 1.41421356;
            }
            if (echoOn) {
                double dl = echoBuf[0][echoIdx], dr = echoBuf[1][echoIdx];
                echoBuf[0][echoIdx] = (float)(l + dl * p.echoFb); echoBuf[1][echoIdx] = (float)(r + dr * p.echoFb);
                if (++echoIdx >= echoLen) echoIdx = 0;
                l += dl * p.echoMix; r += dr * p.echoMix;
            }
            if (rvOn) {
                float in = (float)((l + r) * 0.015), ol = 0, orr = 0;
                for (int k = 0; k < 8; k++) { ol += rv.cl[k].process(in, rvFb, rvD1, rvD2); orr += rv.cr[k].process(in, rvFb, rvD1, rvD2); }
                for (int k = 0; k < 4; k++) { ol = rv.al[k].process(ol); orr = rv.ar[k].process(orr); }
                double nl = p.rvDry * l + ol * rvW1 + orr * rvW2, nr = p.rvDry * r + orr * rvW1 + ol * rvW2; l = nl; r = nr;
            }
            if (cOn) {
                double lvl = lin2db(std::max(std::fabs(l), std::fabs(r))), over = lvl - p.cThr, gr = 0;
                if (2 * over < -p.cKnee) gr = 0;
                else if (2 * std::fabs(over) <= p.cKnee) gr = cSlope * (over + p.cKnee / 2) * (over + p.cKnee / 2) / (2 * std::max(p.cKnee, 1e-6));
                else gr = cSlope * over;
                double co = gr < cEnv ? cAtk : cRel; cEnv = gr + co * (cEnv - gr);
                double g = db2lin(cEnv) * cMake; l *= g; r *= g;
            }
            vCur += (vTarget - vCur) * vk; if (std::fabs(vTarget - vCur) < 1e-6) vCur = vTarget;
            l *= vCur; r *= vCur;
            if (limOn) {
                double pk = std::max(std::fabs(l), std::fabs(r)), tg = pk > limCeil ? limCeil / pk : 1.0;
                if (tg < limG) limG = tg; else limG = 1.0 - (1.0 - limG) * limRelCoef;
                l *= limG; r *= limG;
            }
            pcm[2 * i] = (float)clampv(l, -1.0, 1.0); pcm[2 * i + 1] = (float)clampv(r, -1.0, 1.0);
        }
    }
};
} // namespace

extern "C" {
void*    aw_dsp_new(uint32_t sr) { return new Dsp((double)sr); }
void     aw_dsp_free(void* d) { delete static_cast<Dsp*>(d); }
void     aw_dsp_reset_params(void* d) { static_cast<Dsp*>(d)->p = Params(); }
int      aw_dsp_set(void* d, const char* key, double v) { return static_cast<Dsp*>(d)->set(key, v); }
void     aw_dsp_commit(void* d) { static_cast<Dsp*>(d)->commit(); }
int      aw_dsp_is_identity(void* d) { return static_cast<Dsp*>(d)->active ? 0 : 1; }
int      aw_dsp_timescale_active(void* d) { return static_cast<Dsp*>(d)->tsOn ? 1 : 0; }
double   aw_dsp_tempo(void* d) { return static_cast<Dsp*>(d)->tempo(); }
void     aw_dsp_reset_state(void* d) { static_cast<Dsp*>(d)->reset_state(); }
void     aw_dsp_process(void* d, float* pcm, uint32_t frames) { static_cast<Dsp*>(d)->process(pcm, frames); }
// Variable-length pre-stage. Writes at most `cap` frames to out; returns frames written.
uint32_t aw_dsp_timescale(void* d, const float* in, uint32_t frames, float* out, uint32_t cap) {
    flush_denormals();
    Dsp* s = static_cast<Dsp*>(d);
    std::vector<float> tmp;
    s->ts.process(in, (int)frames, tmp);
    size_t n = std::min<size_t>(tmp.size() / 2, cap);
    std::memcpy(out, tmp.data(), n * 2 * sizeof(float));
    return (uint32_t)n;
}
} // extern "C"
