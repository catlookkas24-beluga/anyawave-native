//! Safe wrapper over cpp/dave_bridge.h  (Discord libdave / MLS).
use std::ffi::c_void;
#[repr(C)] pub struct AwDave { _p: [u8; 0] }
extern "C" {
    fn aw_dave_new(user: u64, channel: u64) -> *mut AwDave;
    fn aw_dave_free(d: *mut AwDave);
    fn aw_dave_init(d: *mut AwDave, ver: u16) -> i32;
    fn aw_dave_set_external_sender(d: *mut AwDave, p: *const u8, n: usize);
    fn aw_dave_key_package(d: *mut AwDave, out: *mut u8, cap: usize) -> i32;
    fn aw_dave_process_commit(d: *mut AwDave, p: *const u8, n: usize) -> i32;
    fn aw_dave_execute_transition(d: *mut AwDave, id: u16);
    fn aw_dave_max_ct(d: *mut AwDave, n: usize) -> usize;
    fn aw_dave_encrypt(d: *mut AwDave, ssrc: u32, i: *const u8, n: usize, o: *mut u8, cap: usize) -> i32;
}
pub struct Dave { raw: *mut AwDave, pub version: u16 }
unsafe impl Send for Dave {}
impl Dave {
    pub fn new(user: u64, channel: u64) -> Self { Self { raw: unsafe { aw_dave_new(user, channel) }, version: 0 } }
    pub fn init(&mut self, v: u16) { self.version = v; unsafe { aw_dave_init(self.raw, v); } }
    pub fn set_external_sender(&mut self, d: &[u8]) { unsafe { aw_dave_set_external_sender(self.raw, d.as_ptr(), d.len()) } }
    pub fn key_package(&mut self) -> Vec<u8> {
        let mut b = vec![0u8; 4096];
        let n = unsafe { aw_dave_key_package(self.raw, b.as_mut_ptr(), b.len()) };
        b.truncate(n.max(0) as usize); b
    }
    pub fn process_commit(&mut self, d: &[u8]) -> bool { unsafe { aw_dave_process_commit(self.raw, d.as_ptr(), d.len()) == 0 } }
    pub fn execute_transition(&mut self, id: u16) { unsafe { aw_dave_execute_transition(self.raw, id) } }
    /// hot path: called once per 20 ms per player, writes into a reusable buffer
    pub fn encrypt(&mut self, ssrc: u32, frame: &[u8], out: &mut Vec<u8>) -> bool {
        if self.version == 0 { out.clear(); out.extend_from_slice(frame); return true; }
        let cap = unsafe { aw_dave_max_ct(self.raw, frame.len()) };
        out.resize(cap, 0);
        let n = unsafe { aw_dave_encrypt(self.raw, ssrc, frame.as_ptr(), frame.len(), out.as_mut_ptr(), cap) };
        if n < 0 { return false; } out.truncate(n as usize); true
    }
}
impl Drop for Dave { fn drop(&mut self) { unsafe { aw_dave_free(self.raw) } } }
