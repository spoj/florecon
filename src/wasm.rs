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
use std::cell::RefCell;
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

// ---------------------------------------------------------------------------
// Stateful interactive surface: one workspace, driven by JSON commands.
// ---------------------------------------------------------------------------

use crate::api::{Plan, Row, Schema, Workspace, WorkspaceReport};
use crate::recon::ExtId;

thread_local! {
    static WS: RefCell<Option<Workspace>> = const { RefCell::new(None) };
}

/// An interactive command. The host ships one of these as JSON to [`dispatch`].
#[derive(serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Cmd {
    /// Create (or replace) the workspace with a schema, plan, and initial rows.
    Init {
        schema: Schema,
        plan: Plan,
        #[serde(default)]
        rows: Vec<(ExtId, Row)>,
    },
    /// Insert or replace rows.
    Upsert { rows: Vec<(ExtId, Row)> },
    /// Remove rows by id.
    Remove { ids: Vec<ExtId> },
    /// Recompute the unfrozen pool.
    Solve,
    /// Lock a group so re-solves leave it intact.
    Freeze { group_id: u64 },
    /// Freeze every clean (|net| <= tol) live group in one shot.
    FreezeClean { tol: i64 },
    /// Unlock a frozen group.
    Unfreeze { group_id: u64 },
    /// Dissolve a group back to the residual.
    Breakup { group_id: u64 },
    /// Return the current state without recomputing.
    Report,
}

#[derive(serde::Serialize)]
struct WsEnvelope {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    report: Option<WorkspaceReport>,
}

impl WsEnvelope {
    fn ok(report: WorkspaceReport) -> Self {
        WsEnvelope {
            ok: true,
            error: None,
            report: Some(report),
        }
    }
    fn err(msg: String) -> Self {
        WsEnvelope {
            ok: false,
            error: Some(msg),
            report: None,
        }
    }
}

/// Run one JSON [`Cmd`] against the persistent workspace and return a JSON
/// [`WsEnvelope`] packed as `(len << 32) | ptr`.
///
/// # Safety
/// `ptr`/`len` must name a readable buffer in this module's memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dispatch(ptr: u32, len: u32) -> u64 {
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let mut out = dispatch_json(bytes);
    out.shrink_to_fit();
    let n = out.len() as u64;
    let mut out = ManuallyDrop::new(out);
    let p = out.as_mut_ptr() as u64;
    (n << 32) | p
}

fn dispatch_json(bytes: &[u8]) -> Vec<u8> {
    let cmd: Cmd = match serde_json::from_slice(bytes) {
        Ok(c) => c,
        Err(e) => return enc(WsEnvelope::err(format!("bad command json: {e}"))),
    };
    WS.with(|cell| {
        let mut slot = cell.borrow_mut();
        let env = apply(&mut slot, cmd);
        enc(env)
    })
}

fn apply(slot: &mut Option<Workspace>, cmd: Cmd) -> WsEnvelope {
    // Init is the only command that may run without an existing workspace.
    if let Cmd::Init { schema, plan, rows } = cmd {
        let mut ws = Workspace::new(schema, plan);
        for (id, row) in rows {
            if let Err(e) = ws.upsert(id, row) {
                return WsEnvelope::err(e.to_string());
            }
        }
        let rep = ws.report();
        *slot = Some(ws);
        return WsEnvelope::ok(rep);
    }
    let ws = match slot.as_mut() {
        Some(ws) => ws,
        None => return WsEnvelope::err("no workspace: send init first".into()),
    };
    let result = match cmd {
        Cmd::Init { .. } => unreachable!(),
        Cmd::Upsert { rows } => rows
            .into_iter()
            .try_for_each(|(id, row)| ws.upsert(id, row)),
        Cmd::Remove { ids } => {
            ids.into_iter().for_each(|id| ws.remove(id));
            Ok(())
        }
        Cmd::Solve => ws.solve(),
        Cmd::Freeze { group_id } => ws.freeze(group_id),
        Cmd::FreezeClean { tol } => {
            ws.freeze_clean(tol);
            Ok(())
        }
        Cmd::Unfreeze { group_id } => ws.unfreeze(group_id),
        Cmd::Breakup { group_id } => ws.breakup(group_id),
        Cmd::Report => Ok(()),
    };
    match result {
        Ok(()) => WsEnvelope::ok(ws.report()),
        Err(e) => WsEnvelope::err(e.to_string()),
    }
}

fn enc(env: WsEnvelope) -> Vec<u8> {
    serde_json::to_vec(&env).unwrap_or_else(|_| br#"{"ok":false,"error":"serialize"}"#.to_vec())
}
