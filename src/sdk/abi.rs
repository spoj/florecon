//! The wasm ABI driver and the `export_plugin!` macro.
//!
//! `export_plugin!` emits the C-ABI exports (`abi_version`, `alloc`, `dealloc`,
//! `describe`, `dispatch`) and a thread-local [`Session`], all generic over the
//! author's [`Plugin`]. The command set is *planless* — the plugin already owns
//! the strategy — so there is no `init{plan}` / `replan`.

use std::collections::BTreeSet;
use std::mem::ManuallyDrop;

use serde::{Deserialize, Serialize};

use crate::ExtId;
use crate::recon::Recon;
use crate::strategy::Allocation;
use crate::report::Report;
use crate::sdk::plugin::Plugin;
use crate::sdk::table::Table;

/// A live reconciliation session: the author's plugin plus the stateful
/// [`Recon`] it drives. One per wasm instance.
pub struct Session<P: Plugin> {
    plugin: P,
    recon: Recon<P::Row>,
}

impl<P: Plugin + 'static> Session<P> {
    pub fn new() -> Self {
        let plugin = P::new();
        let recon = Recon::new(plugin.strategy(), P::primary);
        Session { plugin, recon }
    }

    /// Decode the host's Arrow table and upsert every row under its stable id.
    fn feed(&mut self, arrow: &[u8]) -> Result<(), String> {
        let table = Table::from_ipc(arrow)?;
        let mut seen: BTreeSet<ExtId> = BTreeSet::new();
        for i in 0..table.len() {
            let rv = table.row(i);
            let id = self.plugin.id(&rv);
            if !seen.insert(id) {
                return Err(format!(
                    "duplicate identity: two rows in this batch hash to id {id} (non-unique key)"
                ));
            }
            let row = self.plugin.project(&rv);
            self.recon.upsert(id, row);
        }
        Ok(())
    }
}

impl<P: Plugin + 'static> Default for Session<P> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Serialize)]
struct Envelope {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    report: Option<Report>,
}

impl Envelope {
    fn ok(report: Report) -> Self {
        Envelope { ok: true, error: None, report: Some(report) }
    }
    fn err(msg: String) -> Self {
        Envelope { ok: false, error: Some(msg), report: None }
    }
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Cmd {
    /// (Re)open the session and ingest any rows in the batch.
    Init,
    /// Ingest more rows (carried in the Arrow batch).
    Upsert,
    Remove { ids: Vec<ExtId> },
    Solve,
    Freeze { group_id: u64 },
    FreezeClean { tol: i64 },
    FreezeSingletons { ids: Vec<ExtId> },
    Unfreeze { group_id: u64 },
    Breakup { group_id: u64 },
    Group {
        ids: Vec<ExtId>,
        #[serde(default)]
        net: i64,
        #[serde(default)]
        origin: Option<String>,
        #[serde(default)]
        reason: Option<String>,
    },
    GroupAllocations {
        allocations: Vec<Allocation>,
        #[serde(default)]
        origin: Option<String>,
        #[serde(default)]
        reason: Option<String>,
    },
    RemoveAllocations { group_id: u64, ids: Vec<ExtId> },
    Ungroup { ids: Vec<ExtId> },
    Report,
}

/// Run one command against the session slot. Generic over the plugin; the
/// `export_plugin!` macro supplies the thread-local slot.
pub fn dispatch<P: Plugin + 'static>(slot: &mut Option<Session<P>>, cmd_bytes: &[u8], arrow: &[u8]) -> Vec<u8> {
    let cmd: Cmd = match serde_json::from_slice(cmd_bytes) {
        Ok(c) => c,
        Err(e) => return enc(Envelope::err(format!("bad command json: {e}"))),
    };

    if let Cmd::Init = cmd {
        let mut session = Session::<P>::new();
        if let Err(e) = session.feed(arrow) {
            return enc(Envelope::err(e));
        }
        let rep = session.recon.report();
        *slot = Some(session);
        return enc(Envelope::ok(rep));
    }

    let session = match slot.as_mut() {
        Some(s) => s,
        None => return enc(Envelope::err("no session: send init first".into())),
    };

    let result: Result<(), String> = match cmd {
        Cmd::Init => unreachable!(),
        Cmd::Upsert => session.feed(arrow),
        Cmd::Remove { ids } => {
            session.recon.remove_many(&ids);
            Ok(())
        }
        Cmd::Solve => session.recon.solve().map_err(|e| e.to_string()),
        Cmd::Freeze { group_id } => session.recon.freeze(group_id).map_err(|e| e.to_string()),
        Cmd::FreezeClean { tol } => {
            session.recon.freeze_clean(tol);
            Ok(())
        }
        Cmd::FreezeSingletons { ids } => {
            session.recon.freeze_singletons(&ids);
            Ok(())
        }
        Cmd::Unfreeze { group_id } => session.recon.unfreeze(group_id).map_err(|e| e.to_string()),
        Cmd::Breakup { group_id } => session.recon.breakup(group_id).map_err(|e| e.to_string()),
        Cmd::Group { ids, net, origin, reason } => session
            .recon
            .group(&ids, net, origin.as_deref().unwrap_or("manual"), reason)
            .map(|_| ())
            .map_err(|e| e.to_string()),
        Cmd::GroupAllocations { allocations, origin, reason } => session
            .recon
            .group_allocations(&allocations, origin.as_deref().unwrap_or("manual"), reason)
            .map(|_| ())
            .map_err(|e| e.to_string()),
        Cmd::RemoveAllocations { group_id, ids } => {
            session.recon.remove_allocations(group_id, &ids).map_err(|e| e.to_string())
        }
        Cmd::Ungroup { ids } => session.recon.ungroup(&ids).map_err(|e| e.to_string()),
        Cmd::Report => Ok(()),
    };

    match result {
        Ok(()) => enc(Envelope::ok(session.recon.report())),
        Err(e) => enc(Envelope::err(e)),
    }
}

/// Serialize the plugin's self-description (the `describe()` export payload).
pub fn describe_json<P: Plugin>() -> Vec<u8> {
    serde_json::to_vec(&P::describe()).unwrap_or_else(|_| b"{}".to_vec())
}

fn enc(env: Envelope) -> Vec<u8> {
    serde_json::to_vec(&env).unwrap_or_else(|_| br#"{"ok":false,"error":"serialize"}"#.to_vec())
}

// --- raw memory ABI helpers (used by the macro-generated exports) -----------

/// Allocate `len` bytes of linear memory and return the pointer.
pub fn alloc(len: u32) -> u32 {
    let buf = ManuallyDrop::new(vec![0u8; len as usize]);
    buf.as_ptr() as u32
}

/// Free a buffer previously returned by [`alloc`].
///
/// # Safety
/// `ptr`/`len` must come from a prior [`alloc`] call.
pub unsafe fn dealloc(ptr: u32, len: u32) {
    unsafe { drop(Vec::from_raw_parts(ptr as *mut u8, len as usize, len as usize)) }
}

/// Borrow a host-provided buffer as a slice.
///
/// # Safety
/// `ptr`/`len` must describe a valid readable region in linear memory.
pub unsafe fn slice<'a>(ptr: u32, len: u32) -> &'a [u8] {
    if len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) }
    }
}

/// Leak a byte buffer to the host and pack it into a single `u64` as
/// `(len << 32) | ptr`.
pub fn ret_bytes(mut out: Vec<u8>) -> u64 {
    out.shrink_to_fit();
    let n = out.len() as u64;
    let mut out = ManuallyDrop::new(out);
    let p = out.as_mut_ptr() as u64;
    (n << 32) | p
}

/// Emit the wasm ABI for a [`Plugin`]: `abi_version`, `alloc`, `dealloc`,
/// `describe`, and `dispatch`, plus a thread-local [`Session`].
#[macro_export]
macro_rules! export_plugin {
    ($t:ty) => {
        thread_local! {
            static __FLORECON_SESSION: ::core::cell::RefCell<
                ::core::option::Option<$crate::sdk::Session<$t>>,
            > = const { ::core::cell::RefCell::new(::core::option::Option::None) };
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn abi_version() -> u32 {
            $crate::sdk::ABI_VERSION
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn alloc(len: u32) -> u32 {
            $crate::sdk::abi::alloc(len)
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn dealloc(ptr: u32, len: u32) {
            unsafe { $crate::sdk::abi::dealloc(ptr, len) }
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn describe() -> u64 {
            $crate::sdk::abi::ret_bytes($crate::sdk::abi::describe_json::<$t>())
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn dispatch(ptr: u32, len: u32, arrow_ptr: u32, arrow_len: u32) -> u64 {
            let cmd = unsafe { $crate::sdk::abi::slice(ptr, len) };
            let arrow = unsafe { $crate::sdk::abi::slice(arrow_ptr, arrow_len) };
            let out = __FLORECON_SESSION.with(|cell| {
                $crate::sdk::abi::dispatch::<$t>(&mut cell.borrow_mut(), cmd, arrow)
            });
            $crate::sdk::abi::ret_bytes(out)
        }
    };
}
