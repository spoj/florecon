use crate::plan::Report;
use std::cell::RefCell;
use std::mem::ManuallyDrop;

#[unsafe(no_mangle)]
pub extern "C" fn abi_version() -> u32 {
    crate::plan::CONTRACT_VERSION
}

#[unsafe(no_mangle)]
pub extern "C" fn alloc(len: u32) -> u32 {
    let buf = vec![0u8; len as usize];
    let mut buf = ManuallyDrop::new(buf);
    buf.as_mut_ptr() as u32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn dealloc(ptr: u32, len: u32) {
    unsafe {
        drop(Vec::from_raw_parts(
            ptr as *mut u8,
            len as usize,
            len as usize,
        ));
    }
}

#[derive(serde::Serialize)]
struct Envelope {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    report: Option<Report>,
}

use crate::flow::ExtId;
use crate::plan::{AllocationSpec, Plan, Workspace};

thread_local! {
    static WS: RefCell<Option<Workspace>> = const { RefCell::new(None) };
}

#[derive(serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Cmd {
    Init {
        plan: Plan,
    },
    Replan {
        plan: Plan,
    },
    Upsert {},
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
        allocations: Vec<AllocationSpec>,
        #[serde(default)]
        origin: Option<String>,
        #[serde(default)]
        reason: Option<String>,
    },
    RemoveAllocations { group_id: u64, ids: Vec<ExtId> },
    Ungroup { ids: Vec<ExtId> },
    Report,
}

impl Envelope {
    fn ok(report: Report) -> Self {
        Envelope {
            ok: true,
            error: None,
            report: Some(report),
        }
    }
    fn err(msg: String) -> Self {
        Envelope {
            ok: false,
            error: Some(msg),
            report: None,
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn dispatch(ptr: u32, len: u32, arrow_ptr: u32, arrow_len: u32) -> u64 {
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let arrow_bytes = if arrow_len > 0 {
        unsafe { std::slice::from_raw_parts(arrow_ptr as *const u8, arrow_len as usize) }
    } else {
        &[]
    };
    let mut out = dispatch_json(bytes, arrow_bytes);
    out.shrink_to_fit();
    let n = out.len() as u64;
    let mut out = ManuallyDrop::new(out);
    let p = out.as_mut_ptr() as u64;
    (n << 32) | p
}

fn dispatch_json(bytes: &[u8], arrow_bytes: &[u8]) -> Vec<u8> {
    let cmd: Cmd = match serde_json::from_slice(bytes) {
        Ok(c) => c,
        Err(e) => return enc(Envelope::err(format!("bad command json: {e}"))),
    };
    WS.with(|cell| {
        let mut slot = cell.borrow_mut();
        let env = apply(&mut slot, cmd, arrow_bytes);
        enc(env)
    })
}

fn apply(slot: &mut Option<Workspace>, cmd: Cmd, arrow_bytes: &[u8]) -> Envelope {
    // `init` establishes the session: the batch schema *is* the column map, so
    // we derive the map here and seed the workspace with whatever rows came in
    // the same batch (possibly none — a schema-only init opens an empty book).
    if let Cmd::Init { plan } = cmd {
        let (ids, rows, map) = match crate::arrow::rows_from_ipc(arrow_bytes) {
            Ok(parsed) => parsed,
            Err(e) => return Envelope::err(e.to_string()),
        };
        let mut ws = match Workspace::new(map, plan) {
            Ok(ws) => ws,
            Err(e) => return Envelope::err(e.to_string()),
        };
        for (id, row) in ids.into_iter().zip(rows) {
            ws.upsert(id, row);
        }
        let rep = ws.report();
        *slot = Some(ws);
        return Envelope::ok(rep);
    }

    let ws = match slot.as_mut() {
        Some(ws) => ws,
        None => return Envelope::err("no workspace: send init first".into()),
    };
    let result = match cmd {
        Cmd::Init { .. } => unreachable!("handled above"),
        // Recompile a new plan against the live schema and swap it in, keeping
        // rows + frozen decisions. The next `solve` re-matches under it.
        Cmd::Replan { plan } => ws.replan(plan),
        // Incremental rows lower against the live map by column name, so an
        // upsert batch is order-independent and validates its columns.
        Cmd::Upsert {} => match crate::arrow::rows_from_ipc_mapped(arrow_bytes, ws.map()) {
            Ok((ids, rows)) => {
                for (id, row) in ids.into_iter().zip(rows) {
                    ws.upsert(id, row);
                }
                Ok(())
            }
            Err(e) => Err(e),
        },
        Cmd::Remove { ids } => {
            ws.remove_many(&ids);
            Ok(())
        }
        Cmd::Solve => ws.solve(),
        Cmd::Freeze { group_id } => ws.freeze(group_id),
        Cmd::FreezeClean { tol } => {
            ws.freeze_clean(tol);
            Ok(())
        }
        Cmd::FreezeSingletons { ids } => {
            ws.freeze_singletons(&ids);
            Ok(())
        }
        Cmd::Unfreeze { group_id } => ws.unfreeze(group_id),
        Cmd::Breakup { group_id } => ws.breakup(group_id),
        Cmd::Group { ids, net, origin, reason } => ws
            .group(&ids, net, origin.as_deref().unwrap_or("manual"), reason)
            .map(|_| ()),
        Cmd::GroupAllocations {
            allocations,
            origin,
            reason,
        } => ws
            .group_allocations(&allocations, origin.as_deref().unwrap_or("manual"), reason)
            .map(|_| ()),
        Cmd::RemoveAllocations { group_id, ids } => ws.remove_allocations(group_id, &ids),
        Cmd::Ungroup { ids } => ws.ungroup(&ids),
        Cmd::Report => Ok(()),
    };
    match result {
        Ok(()) => Envelope::ok(ws.report()),
        Err(e) => Envelope::err(e.to_string()),
    }
}

fn enc(env: Envelope) -> Vec<u8> {
    serde_json::to_vec(&env).unwrap_or_else(|_| br#"{"ok":false,"error":"serialize"}"#.to_vec())
}
