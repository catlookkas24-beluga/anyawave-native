//! Safe wrapper over the C++ filter chain (cpp/dsp.cpp).
use std::ffi::c_void;

extern "C" {
    fn aw_dsp_new(sample_rate: u32) -> *mut c_void;
    fn aw_dsp_free(p: *mut c_void);
    fn aw_dsp_set_volume(p: *mut c_void, v: f32);
    fn aw_dsp_set_eq(p: *mut c_void, gains15: *const f32);
    fn aw_dsp_set_tremolo(p: *mut c_void, freq: f32, depth: f32);
    /// In-place, interleaved stereo f32.
    fn aw_dsp_process(p: *mut c_void, pcm: *mut f32, frames: u32);
    fn aw_dsp_is_identity(p: *mut c_void) -> i32;
}

pub struct FilterChain(*mut c_void);
unsafe impl Send for FilterChain {}

impl FilterChain {
    pub fn new(sr: u32) -> Self { Self(unsafe { aw_dsp_new(sr) }) }
    pub fn set_volume(&mut self, v: f32) { unsafe { aw_dsp_set_volume(self.0, v) } }
    pub fn set_eq(&mut self, g: &[f32; 15]) { unsafe { aw_dsp_set_eq(self.0, g.as_ptr()) } }
    pub fn set_tremolo(&mut self, f: f32, d: f32) { unsafe { aw_dsp_set_tremolo(self.0, f, d) } }
    pub fn process(&mut self, pcm: &mut [f32]) {
        unsafe { aw_dsp_process(self.0, pcm.as_mut_ptr(), (pcm.len() / 2) as u32) }
    }
    /// true => no filter active, Opus passthrough allowed (zero decode/encode cost).
    pub fn is_identity(&self) -> bool { unsafe { aw_dsp_is_identity(self.0) != 0 } }
}
impl Drop for FilterChain { fn drop(&mut self) { unsafe { aw_dsp_free(self.0) } } }
