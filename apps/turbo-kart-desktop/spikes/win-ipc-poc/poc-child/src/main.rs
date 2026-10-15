//! E7.WINDOWS.1-B Phase 1: Windows private child-process IPC proof of concept
//! (owner decisions D12 option B, D3 amended). Not shipped; nothing in the app
//! uses it.
//!
//! Roles (first argument):
//!
//! - `child`: stands in for `kart-net-sidecar`. Reads the one-line session key
//!   from stdin (as today), then protocol v1 frames (4-byte big-endian length +
//!   JSON) from stdin and writes frames to stdout. Logs only to stderr; lines
//!   starting `POC-DIAG ` carry JSON diagnostics for the harness. On Windows it
//!   first inventories the handles it inherited and inspects its stdio pipes
//!   (type, end, instance count, server process, name, security descriptor).
//! - `probe`: an unrelated process (run as the same user, another user, or via
//!   the SMB redirector paths) trying to reach the host's pipes and processes.
//! - `squat`: races libuv's pipe-creation window on the predictable names.
//!
//! Exit codes (`child`): 0 clean, 2 usage/bad key, 3 protocol error.

use serde_json::{json, Value};
use std::io::{BufRead, Write};

const MAX_FRAME: usize = 64 * 1024;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("child") => child(&args[1..]),
        Some("probe") => probe(&args[1..]),
        Some("squat") => squat(&args[1..]),
        _ => {
            eprintln!("usage: win-ipc-poc child --mode <m> --host-pid <pid> | probe --pid <host pid> [--child-pid <pid>] [--out <file>] | squat --pid <host pid> --ms <n>");
            std::process::exit(2);
        }
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn diag(v: Value) {
    let mut e = std::io::stderr().lock();
    let _ = writeln!(e, "POC-DIAG {v}");
}

fn exit(code: i32) -> ! {
    let _ = std::io::stderr().flush();
    std::process::exit(code)
}

// ------------------------------------------------------------------ child

fn child(args: &[String]) {
    // Before anything else opens handles: what did this process inherit?
    #[cfg(windows)]
    diag(win::inventory(flag(args, "--host-pid").and_then(|p| p.parse().ok())));
    #[cfg(windows)]
    if !args.iter().any(|a| a == "--keep-inherit") {
        // Phase 2 hardening candidate: the control handles must not reach any
        // process this one might start.
        diag(win::clear_stdio_inherit());
    }
    let mode = flag(args, "--mode").unwrap_or_else(|| "ok".into());
    let crash_at: u64 = flag(args, "--crash-at").and_then(|v| v.parse().ok()).unwrap_or(3);
    diag(json!({ "event": "child_start", "pid": std::process::id(), "mode": mode }));

    let mut key = String::new();
    if std::io::stdin().lock().read_line(&mut key).is_err() {
        exit(2);
    }
    let key = key.trim().to_string();
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        eprintln!("win-ipc-poc: expected a 32-byte hex session key on stdin");
        exit(2);
    }
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime");
    let code = rt.block_on(serve(mode, key, crash_at));
    // A blocking stdin read may still be pending in tokio's pool: do not wait for it.
    exit(code)
}

fn encode(v: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(v).expect("json");
    let mut out = (body.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(&body);
    out
}

async fn serve(mode: String, key: String, crash_at: u64) -> i32 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // The real sidecar would use tokio's stdin/stdout the same way: blocking
    // reads/writes on tokio's blocking pool (works for any pipe kind).
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        while let Some(b) = rx.recv().await {
            if out.write_all(&b).await.is_err() || out.flush().await.is_err() {
                break;
            }
        }
    });
    let signing_key = format!("uhCAk{}", "A".repeat(48));
    match mode.as_str() {
        "silent" => {}
        "garbage" => drop(tx.send(b"this is not a protocol frame\r\n".to_vec()).await),
        "oversized" => drop(tx.send(vec![0xff, 0xff, 0xff, 0xff, b'{']).await),
        "wrong-key" => {
            let mut wrong: Vec<u8> = key.bytes().collect();
            wrong[0] = if wrong[0] == b'0' { b'1' } else { b'0' };
            let hello = json!({ "type": "hello", "protocol": 1, "session_key": String::from_utf8(wrong).unwrap(), "signing_key": signing_key });
            drop(tx.send(encode(&hello)).await);
        }
        _ => {
            let hello = json!({ "type": "hello", "protocol": 1, "session_key": key, "signing_key": signing_key });
            drop(tx.send(encode(&hello)).await);
        }
    }
    if mode == "stderr-flood" {
        // Ordinary logs on stderr while the control channel is busy.
        std::thread::spawn(|| {
            let line = "x".repeat(1000);
            for i in 0..20_000 {
                let mut e = std::io::stderr().lock();
                if writeln!(e, "log line {i} {line}").is_err() {
                    break;
                }
            }
        });
    }

    let mut input = tokio::io::stdin();
    let mut after_key: u64 = 0;
    let mut frames: u64 = 0;
    let mut requests: u64 = 0;
    let mut provisioned = false;
    let report = |reason: &str, after_key: u64, frames: u64, provisioned: bool| {
        diag(json!({ "event": "child_exit", "reason": reason, "bytes_after_key": after_key, "frames": frames, "provision_received": provisioned }));
    };
    loop {
        let mut head = [0u8; 4];
        if let Err(e) = input.read_exact(&mut head).await {
            report(&format!("stdin closed ({})", e.kind()), after_key, frames, provisioned);
            return 0;
        }
        after_key += 4;
        let len = u32::from_be_bytes(head) as usize;
        if len > MAX_FRAME {
            report("oversized frame from host", after_key, frames, provisioned);
            return 3;
        }
        let mut body = vec![0u8; len];
        if input.read_exact(&mut body).await.is_err() {
            report("stdin closed mid-frame", after_key, frames, provisioned);
            return 3;
        }
        after_key += len as u64;
        frames += 1;
        let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
            report("malformed frame from host", after_key, frames, provisioned);
            return 3;
        };
        match msg["type"].as_str() {
            Some("provision") if !provisioned => {
                provisioned = true;
                // Never log the credential itself, only that it arrived.
                diag(json!({ "event": "provision_received", "credential_chars": msg["credential"].as_str().map(str::len) }));
                let _ = tx.send(encode(&json!({ "type": "ready" }))).await;
            }
            Some("request") if provisioned => {
                requests += 1;
                if mode == "crash-after-ready" && requests >= crash_at {
                    diag(json!({ "event": "crashing", "request": requests }));
                    let _ = std::io::stderr().flush();
                    std::process::abort();
                }
                let data = msg["args"]["data"].as_str().unwrap_or("");
                let resp = json!({ "type": "response", "id": msg["id"], "result": { "len": data.len(), "echo": data } });
                let _ = tx.send(encode(&resp)).await;
                if requests % 50 == 0 {
                    let _ = tx.send(encode(&json!({ "type": "event", "event": { "n": requests } }))).await;
                }
            }
            Some("shutdown") if mode != "ignore-shutdown" => {
                report("shutdown", after_key, frames, provisioned);
                // Let the writer drain, as the sidecar does on shutdown.
                drop(tx);
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                return 0;
            }
            Some("shutdown") => diag(json!({ "event": "ignoring_shutdown" })),
            other => {
                report(&format!("unexpected message {other:?}"), after_key, frames, provisioned);
                return 3;
            }
        }
    }
}

// ------------------------------------------------------------------ probe / squat

#[cfg(not(windows))]
fn probe(_: &[String]) {
    println!("{}", json!({ "skipped": "probe needs Windows" }));
}

#[cfg(not(windows))]
fn squat(_: &[String]) {
    println!("{}", json!({ "skipped": "squat needs Windows" }));
}

#[cfg(windows)]
fn probe(args: &[String]) {
    let Some(pid) = flag(args, "--pid").and_then(|p| p.parse::<u32>().ok()) else { exit(2) };
    let child_pid = flag(args, "--child-pid").and_then(|p| p.parse::<u32>().ok());
    let report = win::probe(pid, child_pid);
    let text = report.to_string();
    println!("{text}");
    if let Some(out) = flag(args, "--out") {
        // Never overwrite earlier evidence.
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&out) {
            Ok(mut f) => drop(f.write_all(text.as_bytes())),
            Err(e) => eprintln!("win-ipc-poc: not writing {out}: {e}"),
        }
    }
    exit(if report["opened_any"] == json!(true) { 1 } else { 0 })
}

#[cfg(windows)]
fn squat(args: &[String]) {
    let Some(pid) = flag(args, "--pid").and_then(|p| p.parse::<u32>().ok()) else { exit(2) };
    let ms: u64 = flag(args, "--ms").and_then(|v| v.parse().ok()).unwrap_or(10_000);
    println!("{}", win::squat(pid, ms));
    exit(0)
}

/// Raw Win32 declarations (kernel32/advapi32 only): no extra crate.
#[cfg(windows)]
mod win {
    use serde_json::{json, Value};
    use std::ffi::c_void;
    use std::time::{Duration, Instant};

    type Handle = *mut c_void;
    const INVALID: Handle = -1isize as Handle;
    const STD_HANDLES: [(&str, u32); 3] = [("stdin", -10i32 as u32), ("stdout", -11i32 as u32), ("stderr", -12i32 as u32)];
    const FILE_TYPE_PIPE: u32 = 3;
    const HANDLE_FLAG_INHERIT: u32 = 1;
    const PIPE_SERVER_END: u32 = 1;
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const OPEN_EXISTING: u32 = 3;
    const FILE_NAME_INFO_CLASS: i32 = 2;
    const SE_FILE_OBJECT: i32 = 1;
    const OWNER_GROUP_DACL: u32 = 1 | 2 | 4;
    const PROCESS_VM_READ: u32 = 0x10;
    const PROCESS_DUP_HANDLE: u32 = 0x40;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

    #[repr(C)]
    struct FindData {
        attributes: u32,
        times: [u32; 6],
        size_high: u32,
        size_low: u32,
        reserved: [u32; 2],
        name: [u16; 260],
        alt_name: [u16; 14],
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(n: u32) -> Handle;
        fn GetFileType(h: Handle) -> u32;
        fn GetHandleInformation(h: Handle, flags: *mut u32) -> i32;
        fn SetHandleInformation(h: Handle, mask: u32, flags: u32) -> i32;
        fn GetNamedPipeInfo(h: Handle, flags: *mut u32, out_size: *mut u32, in_size: *mut u32, max_instances: *mut u32) -> i32;
        fn GetNamedPipeHandleStateW(h: Handle, state: *mut u32, cur: *mut u32, max_collect: *mut u32, timeout: *mut u32, user: *mut u16, user_len: u32) -> i32;
        fn GetNamedPipeServerProcessId(h: Handle, pid: *mut u32) -> i32;
        fn GetNamedPipeClientProcessId(h: Handle, pid: *mut u32) -> i32;
        fn GetFileInformationByHandleEx(h: Handle, class: i32, buf: *mut c_void, size: u32) -> i32;
        fn CreateFileW(name: *const u16, access: u32, share: u32, sa: *mut c_void, disposition: u32, flags: u32, template: Handle) -> Handle;
        fn PeekNamedPipe(h: Handle, buf: *mut c_void, size: u32, read: *mut u32, avail: *mut u32, left: *mut u32) -> i32;
        fn ReadFile(h: Handle, buf: *mut c_void, size: u32, read: *mut u32, overlapped: *mut c_void) -> i32;
        fn FindFirstFileW(pattern: *const u16, data: *mut FindData) -> Handle;
        fn FindNextFileW(h: Handle, data: *mut FindData) -> i32;
        fn FindClose(h: Handle) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn CloseHandle(h: Handle) -> i32;
        fn GetLastError() -> u32;
        fn LocalFree(p: *mut c_void) -> *mut c_void;
    }

    #[link(name = "advapi32")]
    extern "system" {
        fn GetSecurityInfo(h: Handle, kind: i32, info: u32, owner: *mut *mut c_void, group: *mut *mut c_void, dacl: *mut *mut c_void, sacl: *mut *mut c_void, sd: *mut *mut c_void) -> u32;
        fn ConvertSecurityDescriptorToStringSecurityDescriptorW(sd: *mut c_void, revision: u32, info: u32, out: *mut *mut u16, len: *mut u32) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn from_wide(w: &[u16]) -> String {
        let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
        String::from_utf16_lossy(&w[..end])
    }

    fn file_name(h: Handle) -> Option<String> {
        // FILE_NAME_INFO: u32 length in bytes, then UTF-16 name.
        let mut buf = vec![0u32; 1 + 1024];
        let ok = unsafe { GetFileInformationByHandleEx(h, FILE_NAME_INFO_CLASS, buf.as_mut_ptr().cast(), (buf.len() * 4) as u32) };
        if ok == 0 {
            return None;
        }
        let bytes = buf[0] as usize;
        let chars = unsafe { std::slice::from_raw_parts(buf.as_ptr().add(1).cast::<u16>(), (bytes / 2).min(2046)) };
        Some(String::from_utf16_lossy(chars))
    }

    fn sddl(h: Handle) -> Value {
        let mut sd: *mut c_void = std::ptr::null_mut();
        let null = std::ptr::null_mut();
        let rc = unsafe { GetSecurityInfo(h, SE_FILE_OBJECT, OWNER_GROUP_DACL, null, null, null, null, &mut sd) };
        if rc != 0 {
            return json!({ "error": rc });
        }
        let mut s: *mut u16 = std::ptr::null_mut();
        let mut len = 0u32;
        let ok = unsafe { ConvertSecurityDescriptorToStringSecurityDescriptorW(sd, 1, OWNER_GROUP_DACL, &mut s, &mut len) };
        let out = if ok != 0 {
            let text = from_wide(unsafe { std::slice::from_raw_parts(s, len as usize) });
            unsafe { LocalFree(s.cast()) };
            json!(text)
        } else {
            json!({ "error": unsafe { GetLastError() } })
        };
        unsafe { LocalFree(sd) };
        out
    }

    fn pipe_details(h: Handle) -> Value {
        let (mut flags, mut out_size, mut in_size, mut max_inst) = (0u32, 0u32, 0u32, 0u32);
        let info = unsafe { GetNamedPipeInfo(h, &mut flags, &mut out_size, &mut in_size, &mut max_inst) } != 0;
        let (mut state, mut cur) = (0u32, 0u32);
        let null = std::ptr::null_mut();
        let have_state = unsafe { GetNamedPipeHandleStateW(h, &mut state, &mut cur, null, null, null, 0) } != 0;
        let (mut server, mut client) = (0u32, 0u32);
        let have_server = unsafe { GetNamedPipeServerProcessId(h, &mut server) } != 0;
        let have_client = unsafe { GetNamedPipeClientProcessId(h, &mut client) } != 0;
        json!({
            "end": if !info { "unknown" } else if flags & PIPE_SERVER_END != 0 { "server" } else { "client" },
            "max_instances": info.then_some(max_inst),
            "current_instances": have_state.then_some(cur),
            "server_pid": have_server.then_some(server),
            "client_pid": have_client.then_some(client),
            "name": file_name(h),
        })
    }

    fn inherit_flag(h: Handle) -> Option<bool> {
        let mut f = 0u32;
        (unsafe { GetHandleInformation(h, &mut f) } != 0).then_some(f & HANDLE_FLAG_INHERIT != 0)
    }

    /// The stdio pipes in detail, and every handle value up to 0x4000 that is
    /// valid in this fresh process (pipes and files named), so an unexpected
    /// inherited handle from the host shows up.
    pub fn inventory(host_pid: Option<u32>) -> Value {
        let mut stdio = serde_json::Map::new();
        let mut stdio_values = Vec::new();
        for (name, n) in STD_HANDLES {
            let h = unsafe { GetStdHandle(n) };
            stdio_values.push(h as usize);
            let ft = unsafe { GetFileType(h) };
            let mut v = json!({ "handle": h as usize, "file_type": ft, "inheritable": inherit_flag(h) });
            if ft == FILE_TYPE_PIPE {
                v["pipe"] = pipe_details(h);
                v["security_descriptor"] = sddl(h);
                v["server_is_host"] = json!(host_pid.is_some() && v["pipe"]["server_pid"] == json!(host_pid));
            }
            stdio.insert(name.into(), v);
        }
        let mut others = Vec::new();
        let mut unknown = 0u32;
        for value in (4usize..=0x4000).step_by(4) {
            let h = value as Handle;
            if stdio_values.contains(&value) || inherit_flag(h).is_none() {
                continue;
            }
            let ft = unsafe { GetFileType(h) };
            if ft == 0 {
                unknown += 1; // events, keys, sections, ... (not files or pipes)
                continue;
            }
            let mut v = json!({ "handle": value, "file_type": ft, "inheritable": inherit_flag(h) });
            if ft == FILE_TYPE_PIPE {
                v["pipe"] = pipe_details(h);
            } else if ft == 1 {
                v["name"] = json!(file_name(h));
            }
            others.push(v);
        }
        let foreign_host_pipes = others.iter().filter(|v| host_pid.is_some() && v["pipe"]["server_pid"] == json!(host_pid)).count();
        json!({ "event": "inventory", "pid": std::process::id(), "host_pid": host_pid, "stdio": stdio, "other_file_or_pipe_handles": others, "other_non_file_handles": unknown, "extra_pipes_from_host": foreign_host_pipes })
    }

    pub fn clear_stdio_inherit() -> Value {
        let mut out = serde_json::Map::new();
        for (name, n) in STD_HANDLES {
            let h = unsafe { GetStdHandle(n) };
            let ok = unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0) } != 0;
            out.insert(name.into(), json!({ "cleared": ok, "inheritable_now": inherit_flag(h) }));
        }
        json!({ "event": "stdio_inherit_cleared", "result": out })
    }

    fn list_pipes() -> Vec<String> {
        let mut names = Vec::new();
        let mut data: FindData = unsafe { std::mem::zeroed() };
        let pattern = wide(r"\\.\pipe\*");
        let h = unsafe { FindFirstFileW(pattern.as_ptr(), &mut data) };
        if h == INVALID {
            return names;
        }
        loop {
            names.push(from_wide(&data.name));
            if unsafe { FindNextFileW(h, &mut data) } == 0 {
                break;
            }
        }
        unsafe { FindClose(h) };
        names
    }

    /// The names libuv 1.52.1 gives stdio pipes: `uv\<n>-<pid>`, where n
    /// starts at 18446744073709551615 (`(uintptr_t) INVALID_HANDLE_VALUE`) and
    /// is incremented on a collision.
    fn predicted(pid: u32) -> Vec<String> {
        let mut v = vec![format!("uv\\{}-{pid}", u64::MAX)];
        v.extend((0..6).map(|n| format!("uv\\{n}-{pid}")));
        v
    }

    fn try_open(path: &str, access: u32) -> Value {
        let w = wide(path);
        let h = unsafe { CreateFileW(w.as_ptr(), access, 0, std::ptr::null_mut(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
        if h == INVALID {
            json!({ "opened": false, "error": unsafe { GetLastError() } })
        } else {
            unsafe { CloseHandle(h) };
            json!({ "opened": true })
        }
    }

    fn try_process(pid: u32) -> Value {
        let mut out = serde_json::Map::new();
        for (name, access) in [("query_limited", PROCESS_QUERY_LIMITED_INFORMATION), ("dup_handle", PROCESS_DUP_HANDLE), ("vm_read", PROCESS_VM_READ)] {
            let h = unsafe { OpenProcess(access, 0, pid) };
            let v = if h.is_null() {
                json!({ "granted": false, "error": unsafe { GetLastError() } })
            } else {
                unsafe { CloseHandle(h) };
                json!({ "granted": true })
            };
            out.insert(name.into(), v);
        }
        Value::Object(out)
    }

    pub fn probe(pid: u32, child_pid: Option<u32>) -> Value {
        let all = list_pipes();
        let suffix = format!("-{pid}");
        let mut targets: Vec<String> = all.iter().filter(|n| n.starts_with("uv\\") && n.ends_with(&suffix)).cloned().collect();
        let listed = targets.clone();
        for p in predicted(pid) {
            if !targets.contains(&p) {
                targets.push(p);
            }
        }
        let computer = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "localhost".into());
        let prefixes = [r"\\.\pipe\".to_string(), r"\\localhost\pipe\".into(), r"\\127.0.0.1\pipe\".into(), format!(r"\\{computer}\pipe\")];
        let mut attempts = Vec::new();
        let mut opened_any = false;
        for t in &targets {
            for p in &prefixes {
                for (aname, access) in [("read", GENERIC_READ), ("write", GENERIC_WRITE), ("read_write", GENERIC_READ | GENERIC_WRITE), ("attributes", FILE_READ_ATTRIBUTES)] {
                    let r = try_open(&format!("{p}{t}"), access);
                    opened_any |= r["opened"] == json!(true);
                    attempts.push(json!({ "path": format!("{p}{t}"), "listed": listed.contains(t), "remote_path": p != r"\\.\pipe\", "access": aname, "result": r }));
                }
            }
        }
        json!({
            "role": "probe",
            "probe_pid": std::process::id(),
            "user": std::env::var("USERNAME").ok(),
            "domain": std::env::var("USERDOMAIN").ok(),
            "host_pid": pid,
            "child_pid": child_pid,
            "turbo_kart_named_pipes": all.iter().filter(|n| n.contains("turbo-kart")).collect::<Vec<_>>(),
            "listed_host_uv_pipes": listed,
            "attempts": attempts,
            "opened_any": opened_any,
            "host_process": try_process(pid),
            "child_process": child_pid.map(try_process),
        })
    }

    /// Spins on the predicted names. Any open that succeeds is held for at most
    /// 1 s and every readable byte is counted (expected: none).
    pub fn squat(pid: u32, ms: u64) -> Value {
        let deadline = Instant::now() + Duration::from_millis(ms);
        let names: Vec<Vec<u16>> = predicted(pid).iter().map(|n| wide(&format!(r"\\.\pipe\{n}"))).collect();
        let (mut attempts, mut wins, mut bytes) = (0u64, 0u64, 0u64);
        let mut errors = std::collections::BTreeMap::<u32, u64>::new();
        let mut win_log = Vec::new();
        while Instant::now() < deadline {
            for n in &names {
                attempts += 1;
                let h = unsafe { CreateFileW(n.as_ptr(), GENERIC_READ, 0, std::ptr::null_mut(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
                if h == INVALID {
                    *errors.entry(unsafe { GetLastError() }).or_default() += 1;
                    continue;
                }
                wins += 1;
                let held = Instant::now();
                let mut got = 0u64;
                let mut end = "held 1 s".to_string();
                while held.elapsed() < Duration::from_secs(1) {
                    let mut avail = 0u32;
                    let null = std::ptr::null_mut();
                    if unsafe { PeekNamedPipe(h, null, 0, null, &mut avail, null) } == 0 {
                        end = format!("error {}", unsafe { GetLastError() });
                        break;
                    }
                    if avail > 0 {
                        let mut buf = vec![0u8; avail as usize];
                        let mut read = 0u32;
                        unsafe { ReadFile(h, buf.as_mut_ptr().cast(), avail, &mut read, std::ptr::null_mut()) };
                        got += read as u64;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                unsafe { CloseHandle(h) };
                bytes += got;
                if win_log.len() < 50 {
                    win_log.push(json!({ "name": String::from_utf16_lossy(&n[..n.len() - 1]), "bytes": got, "ended": end }));
                }
            }
        }
        json!({ "role": "squat", "host_pid": pid, "attempts": attempts, "wins": wins, "bytes_received": bytes, "errors": errors, "wins_detail": win_log })
    }
}
