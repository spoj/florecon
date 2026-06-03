use crate::plan::{Report, SolveRequest};
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

#[unsafe(no_mangle)]
pub unsafe extern "C" fn solve(req_ptr: u32, req_len: u32, arrow_ptr: u32, arrow_len: u32) -> u64 {
    let bytes = unsafe { std::slice::from_raw_parts(req_ptr as *const u8, req_len as usize) };
    let arrow_bytes = if arrow_len > 0 {
        unsafe { std::slice::from_raw_parts(arrow_ptr as *const u8, arrow_len as usize) }
    } else {
        &[]
    };
    let mut out = run(bytes, arrow_bytes);
    out.shrink_to_fit();
    debug_assert_eq!(out.len(), out.capacity());
    let len = out.len() as u64;
    let mut out = ManuallyDrop::new(out);
    let ptr = out.as_mut_ptr() as u64;
    (len << 32) | ptr
}

#[derive(serde::Serialize)]
struct Envelope {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    report: Option<Report>,
}

fn run(bytes: &[u8], arrow_bytes: &[u8]) -> Vec<u8> {
    let env = match serde_json::from_slice::<SolveRequest>(bytes) {
        Err(e) => Envelope::err(format!("bad request json: {e}")),
        Ok(mut req) => {
            let parsed_rows = match crate::arrow::rows_from_ipc(arrow_bytes) {
                Ok((ids, rows, map)) => {
                    req.map = map;
                    ids.into_iter().zip(rows.into_iter()).collect::<Vec<_>>()
                }
                Err(e) => return enc(Envelope::err(e.to_string())),
            };
            
            match req.run(parsed_rows) {
                Ok(report) => Envelope::ok(report),
                Err(e) => Envelope::err(e.to_string()),
            }
        }
    };
    enc(env)
}

use crate::flow::ExtId;
use crate::plan::{AllocationSpec, Plan, Workspace};
use crate::row::{PhysicalRow, ColumnMap};

thread_local! {
    static WS: RefCell<Option<Workspace>> = const { RefCell::new(None) };
}

#[derive(serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Cmd {
    Init {
        #[serde(default)]
        map: ColumnMap,
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
    },
    GroupAllocations {
        allocations: Vec<AllocationSpec>,
        #[serde(default)]
        origin: Option<String>,
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

fn apply(slot: &mut Option<Workspace>, mut cmd: Cmd, arrow_bytes: &[u8]) -> Envelope {
    let mut rows_to_upsert = Vec::new();
    if arrow_bytes.len() > 0 {
        match crate::arrow::rows_from_ipc(arrow_bytes) {
            Ok((ids, arr_rows, parsed_map)) => {
                let parsed_rows = ids.into_iter().zip(arr_rows.into_iter()).collect::<Vec<_>>();
                rows_to_upsert.extend(parsed_rows);
                if let Cmd::Init { map, .. } = &mut cmd {
                    *map = parsed_map;
                }
            }
            Err(e) => return Envelope::err(e.to_string()),
        }
    }

    if let Cmd::Init { map, plan } = cmd {
        let mut ws = match Workspace::new(map, plan) {
            Ok(ws) => ws,
            Err(e) => return Envelope::err(e.to_string()),
        };
        for (id, row) in rows_to_upsert {
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
        Cmd::Init { .. } => unreachable!(),
        Cmd::Upsert {} => { rows_to_upsert
            .into_iter()
            .for_each(|(id, row)| ws.upsert(id, row));
            Ok(())
        }
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
        Cmd::FreezeSingletons { ids } => {
            ws.freeze_singletons(&ids);
            Ok(())
        }
        Cmd::Unfreeze { group_id } => ws.unfreeze(group_id),
        Cmd::Breakup { group_id } => ws.breakup(group_id),
        Cmd::Group { ids, net, origin } => ws
            .group(&ids, net, origin.as_deref().unwrap_or("manual"))
            .map(|_| ()),
        Cmd::GroupAllocations {
            allocations,
            origin,
        } => ws
            .group_allocations(&allocations, origin.as_deref().unwrap_or("manual"))
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
