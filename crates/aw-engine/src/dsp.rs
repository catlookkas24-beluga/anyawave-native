//! Safe wrapper over the C++ filter chain (cpp/dsp.cpp). Control plane = string keys, data plane = two calls.
use std::ffi::{c_char, c_void, CString};

extern "C" {
    fn aw_dsp_new(sr: u32) -> *mut c_void;
    fn aw_dsp_free(p: *mut c_void);
    fn aw_dsp_reset_params(p: *mut c_void);
    fn aw_dsp_set(p: *mut c_void, key: *const c_char, v: f64) -> i32;
    fn aw_dsp_commit(p: *mut c_void);
    fn aw_dsp_is_identity(p: *mut c_void) -> i32;
    fn aw_dsp_timescale_active(p: *mut c_void) -> i32;
    fn aw_dsp_tempo(p: *mut c_void) -> f64;
    fn aw_dsp_reset_state(p: *mut c_void);
    fn aw_dsp_process(p: *mut c_void, pcm: *mut f32, frames: u32);
    fn aw_dsp_timescale(p: *mut c_void, input: *const f32, frames: u32, out: *mut f32, cap: u32) -> u32;
}

pub struct FilterChain(*mut c_void);
unsafe impl Send for FilterChain {}

impl FilterChain {
    pub fn new(sr: u32) -> Self { Self(unsafe { aw_dsp_new(sr) }) }
    /// back to "no filters" (delay lines / biquad memory are kept, so changes do not click)
    pub fn reset_params(&mut self) { unsafe { aw_dsp_reset_params(self.0) } }
    /// returns false for an unknown key
    pub fn set(&mut self, key: &str, v: f64) -> bool {
        match CString::new(key) { Ok(c) => unsafe { aw_dsp_set(self.0, c.as_ptr(), v) == 0 }, Err(_) => false }
    }
    /// recompute coefficients; call once after a batch of `set`
    pub fn commit(&mut self) { unsafe { aw_dsp_commit(self.0) } }
    /// clear filter memory (call on seek)
    pub fn reset_state(&mut self) { unsafe { aw_dsp_reset_state(self.0) } }
    /// fixed-size stage, interleaved stereo f32, in place
    pub fn process(&mut self, pcm: &mut [f32]) { unsafe { aw_dsp_process(self.0, pcm.as_mut_ptr(), (pcm.len() / 2) as u32) } }
    /// true => no filter at all is active, Opus passthrough is allowed (zero decode/encode cost)
    pub fn is_identity(&self) -> bool { unsafe { aw_dsp_is_identity(self.0) != 0 } }
    /// media seconds that pass per second of output (speed*rate); 1.0 without timescale
    pub fn tempo(&self) -> f64 { unsafe { aw_dsp_tempo(self.0) } }
    /// variable-length pre-stage (speed / pitch / rate). Returns false when inactive: use `input` unchanged.
    pub fn timescale(&mut self, input: &[f32], out: &mut Vec<f32>) -> bool {
        if unsafe { aw_dsp_timescale_active(self.0) } == 0 { return false; }
        let frames = input.len() / 2;
        let cap = frames * 8 + 4096;
        out.resize(cap * 2, 0.0);
        let n = unsafe { aw_dsp_timescale(self.0, input.as_ptr(), frames as u32, out.as_mut_ptr(), cap as u32) };
        out.truncate(n as usize * 2);
        true
    }
}
impl Drop for FilterChain { fn drop(&mut self) { unsafe { aw_dsp_free(self.0) } } }
