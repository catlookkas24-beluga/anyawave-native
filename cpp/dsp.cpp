// Filter chain: volume, 15-band EQ (Lavalink-compatible band centres), tremolo.
// Allocation-free in process(); designed to auto-vectorise. Interleaved stereo f32.
#include <cmath>
#include <cstdint>
#include <cstring>

namespace {
constexpr float kPi = 3.14159265358979f;
constexpr float kBands[15] = {25,40,63,100,160,250,400,630,1000,1600,2500,4000,6300,10000,16000};

struct Biquad {
    float b0=1,b1=0,b2=0,a1=0,a2=0;
    float z1[2]={0,0}, z2[2]={0,0};
    void peaking(float sr, float f, float gain_db, float q) {
        float A = std::pow(10.f, gain_db / 40.f), w = 2*kPi*f/sr, al = std::sin(w)/(2*q), c = std::cos(w);
        float a0 = 1 + al/A;
        b0 = (1 + al*A)/a0; b1 = (-2*c)/a0; b2 = (1 - al*A)/a0; a1 = (-2*c)/a0; a2 = (1 - al/A)/a0;
    }
    inline float run(int ch, float x) {           // transposed direct form II
        float y = b0*x + z1[ch];
        z1[ch] = b1*x - a1*y + z2[ch];
        z2[ch] = b2*x - a2*y;
        return y;
    }
};

struct Dsp {
    float sr, volume = 1.f;
    Biquad eq[15];
    bool eq_on = false;
    float trem_f = 0, trem_d = 0, trem_ph = 0;
    explicit Dsp(float s) : sr(s) {}
};
} // namespace

extern "C" {
void* aw_dsp_new(uint32_t sr) { return new Dsp((float)sr); }
void  aw_dsp_free(void* p) { delete static_cast<Dsp*>(p); }
void  aw_dsp_set_volume(void* p, float v) { static_cast<Dsp*>(p)->volume = v; }

void aw_dsp_set_eq(void* p, const float* gains15) {
    auto* d = static_cast<Dsp*>(p);
    d->eq_on = false;
    for (int i = 0; i < 15; ++i) {
        d->eq[i].peaking(d->sr, kBands[i], gains15[i] * 12.f /*-0.25..1.0 -> dB*/, 1.0f);
        if (gains15[i] != 0.f) d->eq_on = true;
    }
}
void aw_dsp_set_tremolo(void* p, float f, float depth) {
    auto* d = static_cast<Dsp*>(p); d->trem_f = f; d->trem_d = depth;
}
int aw_dsp_is_identity(void* p) {
    auto* d = static_cast<Dsp*>(p);
    return d->volume == 1.f && !d->eq_on && d->trem_d == 0.f;
}

void aw_dsp_process(void* p, float* pcm, uint32_t frames) {
    auto* d = static_cast<Dsp*>(p);
    const bool trem = d->trem_d > 0.f;
    const float inc = 2*kPi*d->trem_f/d->sr;
    for (uint32_t i = 0; i < frames; ++i) {
        float g = d->volume;
        if (trem) { g *= 1.f - d->trem_d*(0.5f + 0.5f*std::sin(d->trem_ph)); d->trem_ph += inc; if (d->trem_ph > 2*kPi) d->trem_ph -= 2*kPi; }
        for (int ch = 0; ch < 2; ++ch) {
            float x = pcm[2*i+ch];
            if (d->eq_on) for (int b = 0; b < 15; ++b) x = d->eq[b].run(ch, x);
            x *= g;
            pcm[2*i+ch] = x > 1.f ? 1.f : (x < -1.f ? -1.f : x);   // hard clip guard before opus
        }
    }
}
} // extern "C"
// TODO: timescale (SoundTouch / Signalsmith Stretch), karaoke, rotation, distortion, low-pass, channel-mix.
