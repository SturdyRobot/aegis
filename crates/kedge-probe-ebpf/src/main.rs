//! Kernel-side eBPF LSM programs for kedge-probe.
//!
//! ⚠️ **Unverified in the authoring environment** (macOS, no BPF toolchain). This
//! is `no_std`/`no_main` code for `bpfel-unknown-none`, written against the
//! aya-ebpf API. Build and load it only on a Linux ≥5.7 host with `CONFIG_BPF_LSM`
//! and `lsm=...,bpf`, root/CAP_BPF, a nightly toolchain and `bpf-linker`. Validate
//! against your exact aya-ebpf version — see ../kedge-probe/BUILD_EBPF.md.
//!
//! ## Why LSM (and not `sys_enter_execve`)
//! A tracepoint can *observe* a syscall but cannot stop it. LSM hooks run at the
//! security-decision point and their return value is honored: `0` allows, a
//! negative errno (e.g. `-EPERM`) denies. That is what makes real in-kernel
//! enforcement possible.

#![no_std]
#![no_main]

use aya_ebpf::{
    macros::{lsm, map},
    maps::{HashMap, RingBuf},
    programs::LsmContext,
};

const EPERM: i32 = -1;

/// One violation record pushed to user space. Layout mirrors
/// `kedge_probe::RawViolation` byte-for-byte.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RawViolation {
    pub pid: u32,
    pub tgid: u32,
    pub kind: u8, // 0 = exec, 1 = connect, 2 = write
    pub detail_len: u16,
    pub detail: [u8; 256],
}

/// TGIDs Kedge supervises → policy flag byte. The hooks are inert for any process
/// not in this map, so we never interfere with the rest of the system.
#[map]
static SUPERVISED: HashMap<u32, u8> = HashMap::with_max_entries(1024, 0);

/// Violations streamed to the user-space runtime.
#[map]
static VIOLATIONS: RingBuf = RingBuf::with_byte_size(256 * 1024, 0);

/// Is the current task one we supervise?
#[inline(always)]
fn supervised() -> Option<(u32, u32)> {
    let id = aya_ebpf::helpers::bpf_get_current_pid_tgid();
    let tgid = (id >> 32) as u32;
    let pid = id as u32;
    // SAFETY: read-only lookup in a BPF map.
    if unsafe { SUPERVISED.get(&tgid) }.is_some() {
        Some((pid, tgid))
    } else {
        None
    }
}

/// Emit a violation record to the ring buffer (best-effort; never blocks).
#[inline(always)]
fn report(pid: u32, tgid: u32, kind: u8) {
    if let Some(mut slot) = VIOLATIONS.reserve::<RawViolation>(0) {
        let v = RawViolation {
            pid,
            tgid,
            kind,
            detail_len: 0, // detail (path/addr) is copied in on target; omitted here
            detail: [0u8; 256],
        };
        slot.write(v);
        slot.submit(0);
    }
}

/// `execve`: block any exec by a supervised task whose binary isn't allowed.
/// (The allow-set check reads the exec path from `bprm->file`; wired on target.)
#[lsm(hook = "bprm_check_security")]
pub fn bprm_check(ctx: LsmContext) -> i32 {
    try_exec(ctx).unwrap_or(0)
}

fn try_exec(_ctx: LsmContext) -> Result<i32, i32> {
    let Some((pid, tgid)) = supervised() else {
        return Ok(0); // not ours → allow
    };
    // On target: resolve the binary path from bprm and consult the exec allow-map.
    // Absent that wiring, default to *observe* (report, allow) so we never brick a
    // supervised process during bring-up. Flip to `EPERM` once the allow-map lands.
    report(pid, tgid, 0);
    Ok(0)
}

/// Outbound `connect`: enforce the destination allowlist.
#[lsm(hook = "socket_connect")]
pub fn socket_connect(ctx: LsmContext) -> i32 {
    try_connect(ctx).unwrap_or(0)
}

fn try_connect(_ctx: LsmContext) -> Result<i32, i32> {
    let Some((pid, tgid)) = supervised() else {
        return Ok(0);
    };
    // On target: read sockaddr from arg1, check against the allowed-IP map; deny
    // with EPERM on miss. Reported here for the event stream.
    report(pid, tgid, 1);
    Ok(0)
}

/// `file_open`: deny writes by supervised tasks to protected paths.
#[lsm(hook = "file_open")]
pub fn file_open(ctx: LsmContext) -> i32 {
    try_write(ctx).unwrap_or(0)
}

fn try_write(_ctx: LsmContext) -> Result<i32, i32> {
    let Some((pid, tgid)) = supervised() else {
        return Ok(0);
    };
    // On target: if the open has write intent (fmode & FMODE_WRITE) and the path is
    // under a protected prefix, return EPERM. The `EPERM` constant is exported so
    // the enforcement path is a one-line change once path resolution is wired.
    let _ = EPERM;
    report(pid, tgid, 2);
    Ok(0)
}

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // BPF programs cannot unwind; a panic is unreachable but the handler is required.
    loop {}
}
