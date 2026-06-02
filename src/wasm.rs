//! WASM consumption surface — a minimal C-ABI over linear memory.
//!
//! No wasm-bindgen: this is a plain `wasm32` module callable from any runtime
//! (wasmtime, browser, Wasmer). The host allocates an input buffer with
//! [`alloc`], writes a [`crate::api::SolveRequest`] as JSON into it, calls
//! [`solve`], reads the returned JSON envelope, then frees both buffers with
//! [`dealloc`]. State stays native; only plans and results cross.
//!
//! The return value of [`solve`] packs `(len << 32) | ptr` into a `u64`
//! (wasm32 pointers are 32-bit), which every runtime can unpack.

use crate::api::{Report, SolveRequest};
use std::mem::ManuallyDrop;

/// Allocate `len` bytes in wasm linear memory; returns the pointer (offset).
/// The host writes its request bytes here before calling [`solve`].
#[unsafe(no_mangle)]
pub extern "C" fn alloc(len: u32) -> u32 {
    let buf = vec![0u8; len as usize];
    let mut buf = ManuallyDrop::new(buf);
    buf.as_mut_ptr() as u32
}

/// Free a buffer previously returned by [`alloc`] or [`solve`].
///
/// # Safety
/// `ptr`/`len` must name a buffer this module handed out and has not freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dealloc(ptr: u32, len: u32) {
    unsafe {
        drop(Vec::from_raw_parts(ptr as *mut u8, len as usize, len as usize));
    }
}

/// Run a JSON [`SolveRequest`] at `[req_ptr, req_ptr+req_len)` and return a
/// JSON envelope buffer packed as `(len << 32) | ptr`.
///
/// # Safety
/// `req_ptr`/`req_len` must name a readable buffer in this module's memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn solve(req_ptr: u32, req_len: u32) -> u64 {
    let bytes = unsafe { std::slice::from_raw_parts(req_ptr as *const u8, req_len as usize) };
    let mut out = run(bytes);
    out.shrink_to_fit();
    debug_assert_eq!(out.len(), out.capacity());
    let len = out.len() as u64;
    let mut out = ManuallyDrop::new(out);
    let ptr = out.as_mut_ptr() as u64;
    (len << 32) | ptr
}

/// A result envelope: `{ ok, error?, report? }`.
#[derive(serde::Serialize)]
struct Envelope {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    report: Option<Report>,
}

fn run(bytes: &[u8]) -> Vec<u8> {
    let env = match serde_json::from_slice::<SolveRequest>(bytes) {
        Err(e) => Envelope {
            ok: false,
            error: Some(format!("bad request json: {e}")),
            report: None,
        },
        Ok(req) => match req.run() {
            Ok(report) => Envelope {
                ok: true,
                error: None,
                report: Some(report),
            },
            Err(e) => Envelope {
                ok: false,
                error: Some(e.to_string()),
                report: None,
            },
        },
    };
    serde_json::to_vec(&env).unwrap_or_else(|_| br#"{"ok":false,"error":"serialize"}"#.to_vec())
}
