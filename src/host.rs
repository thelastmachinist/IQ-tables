//! The boundary with the browser. Browsers only run WebAssembly through a
//! JavaScript loader, so `web/host.js` provides these few primitives (DOM,
//! network, storage, files) and nothing else; all logic — including key
//! handling and signing — lives in Rust.

#[cfg(target_arch = "wasm32")]
mod ffi {
    #[link(wasm_import_module = "host")]
    extern "C" {
        pub fn log(p: *const u8, l: usize);
        pub fn render(p: *const u8, l: usize);
        pub fn fetch(id: u32, mp: *const u8, ml: usize, up: *const u8, ul: usize, bp: *const u8, bl: usize, cp: *const u8, cl: usize);
        pub fn storage_get(kp: *const u8, kl: usize) -> i32;
        pub fn take(p: *mut u8);
        pub fn storage_set(kp: *const u8, kl: usize, vp: *const u8, vl: usize);
        pub fn now() -> f64;
        pub fn random(p: *mut u8, l: usize);
        pub fn download(np: *const u8, nl: usize, mp: *const u8, ml: usize, dp: *const u8, dl: usize);
        pub fn copy(p: *const u8, l: usize);
        pub fn timer(id: u32, ms: u32);
        pub fn set_hash(p: *const u8, l: usize);
        pub fn passkey(id: u32, p: *const u8, l: usize);
        pub fn tz() -> i32;
        pub fn file_read(id: u32, fid: u32, start: f64, len: f64);
        pub fn fetch_bytes(id: u32, up: *const u8, ul: usize, start: f64, len: f64);
        pub fn blob_part(bid: u32, p: *const u8, l: usize);
        pub fn blob_save(bid: u32, np: *const u8, nl: usize, mp: *const u8, ml: usize);
        pub fn blob_drop(bid: u32);
    }
}

#[cfg(target_arch = "wasm32")]
fn staged(len: i32) -> Option<Vec<u8>> {
    if len < 0 {
        return None;
    }
    let mut v = vec![0u8; len as usize];
    unsafe { ffi::take(v.as_mut_ptr()) };
    Some(v)
}

#[cfg(target_arch = "wasm32")]
mod imp {
    use super::{ffi, staged};
    pub fn log(s: &str) {
        unsafe { ffi::log(s.as_ptr(), s.len()) }
    }
    pub fn render(s: &str) {
        unsafe { ffi::render(s.as_ptr(), s.len()) }
    }
    pub fn fetch(id: u32, method: &str, url: &str, body: &str, ctype: &str) {
        unsafe {
            ffi::fetch(id, method.as_ptr(), method.len(), url.as_ptr(), url.len(), body.as_ptr(), body.len(), ctype.as_ptr(), ctype.len())
        }
    }
    pub fn storage_get(k: &str) -> Option<String> {
        let n = unsafe { ffi::storage_get(k.as_ptr(), k.len()) };
        staged(n).and_then(|v| String::from_utf8(v).ok())
    }
    pub fn storage_set(k: &str, v: &str) {
        unsafe { ffi::storage_set(k.as_ptr(), k.len(), v.as_ptr(), v.len()) }
    }
    pub fn now_ms() -> f64 {
        unsafe { ffi::now() }
    }
    /// Minutes the browser's local time is ahead of UTC.
    pub fn tz_offset_min() -> i32 {
        unsafe { ffi::tz() }
    }
    pub fn random(buf: &mut [u8]) {
        unsafe { ffi::random(buf.as_mut_ptr(), buf.len()) }
    }
    pub fn download(name: &str, mime: &str, data: &[u8]) {
        unsafe { ffi::download(name.as_ptr(), name.len(), mime.as_ptr(), mime.len(), data.as_ptr(), data.len()) }
    }
    pub fn copy(s: &str) {
        unsafe { ffi::copy(s.as_ptr(), s.len()) }
    }
    pub fn timer(id: u32, ms: u32) {
        unsafe { ffi::timer(id, ms) }
    }
    pub fn set_hash(s: &str) {
        unsafe { ffi::set_hash(s.as_ptr(), s.len()) }
    }
    /// Create or use a passkey (WebAuthn with the PRF extension); the result
    /// JSON arrives through on_async.
    pub fn passkey(id: u32, req: &str) {
        unsafe { ffi::passkey(id, req.as_ptr(), req.len()) }
    }
    /// Read `len` bytes at `start` of a file the page keeps open; the bytes
    /// arrive through on_async.
    pub fn file_read(id: u32, fid: u32, start: u64, len: u64) {
        unsafe { ffi::file_read(id, fid, start as f64, len as f64) }
    }
    /// GET with a byte range (`len` 0 = the whole thing); raw bytes back.
    pub fn fetch_bytes(id: u32, url: &str, start: u64, len: u64) {
        unsafe { ffi::fetch_bytes(id, url.as_ptr(), url.len(), start as f64, len as f64) }
    }
    /// Downloads built from pieces: append, then save (or drop).
    pub fn blob_part(bid: u32, data: &[u8]) {
        unsafe { ffi::blob_part(bid, data.as_ptr(), data.len()) }
    }
    pub fn blob_save(bid: u32, name: &str, mime: &str) {
        unsafe { ffi::blob_save(bid, name.as_ptr(), name.len(), mime.as_ptr(), mime.len()) }
    }
    pub fn blob_drop(bid: u32) {
        unsafe { ffi::blob_drop(bid) }
    }
}

/// Native stand-ins so the crate builds and unit-tests off the browser.
#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use std::cell::RefCell;
    thread_local! {
        pub static OUT: RefCell<Vec<String>> = RefCell::new(vec![]);
        pub static STORE: RefCell<std::collections::HashMap<String, String>> = RefCell::new(Default::default());
    }
    pub fn log(s: &str) {
        OUT.with(|o| o.borrow_mut().push(format!("log:{}", s)));
    }
    pub fn render(s: &str) {
        OUT.with(|o| o.borrow_mut().push(format!("render:{}", s.len())));
    }
    pub fn fetch(id: u32, method: &str, url: &str, _body: &str, _c: &str) {
        OUT.with(|o| o.borrow_mut().push(format!("fetch:{}:{}:{}", id, method, url)));
    }
    pub fn storage_get(k: &str) -> Option<String> {
        STORE.with(|s| s.borrow().get(k).cloned())
    }
    pub fn storage_set(k: &str, v: &str) {
        STORE.with(|s| s.borrow_mut().insert(k.into(), v.into()));
    }
    pub fn now_ms() -> f64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0)
    }
    pub fn tz_offset_min() -> i32 {
        0
    }
    pub fn random(buf: &mut [u8]) {
        let mut x = now_ms() as u64 | 1;
        for b in buf.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
    }
    pub fn download(_n: &str, _m: &str, _d: &[u8]) {}
    pub fn copy(_s: &str) {}
    pub fn timer(_id: u32, _ms: u32) {}
    pub fn set_hash(_s: &str) {}
    pub fn passkey(id: u32, req: &str) {
        OUT.with(|o| o.borrow_mut().push(format!("passkey:{}:{}", id, req)));
    }
    pub fn file_read(id: u32, fid: u32, start: u64, len: u64) {
        OUT.with(|o| o.borrow_mut().push(format!("file_read:{}:{}:{}:{}", id, fid, start, len)));
    }
    pub fn fetch_bytes(id: u32, url: &str, start: u64, len: u64) {
        OUT.with(|o| o.borrow_mut().push(format!("fetch_bytes:{}:{}:{}:{}", id, url, start, len)));
    }
    pub fn blob_part(_bid: u32, _d: &[u8]) {}
    pub fn blob_save(_bid: u32, _n: &str, _m: &str) {}
    pub fn blob_drop(_bid: u32) {}
}

pub use imp::*;
