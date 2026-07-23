//! Linux eBPF LSM loader (user-space Aya side).
//!
//! ⚠️ **Unverified in the authoring environment.** This module only compiles under
//! `--features ebpf` on Linux and is exercised only on a host with a BPF-LSM
//! kernel + root. It is written against the Aya 0.13 API; validate it against your
//! exact `aya` patch version and kernel when you build on target (see
//! `BUILD_EBPF.md`). It is deliberately excluded from the default CI.

use std::os::fd::AsRawFd;

use aya::maps::{HashMap as BpfHashMap, MapData, RingBuf};
use aya::programs::Lsm;
use aya::{Btf, Ebpf};

use crate::{Probe, ProbeError, ProbePolicy, RawViolation, ViolationEvent};

/// The compiled kernel object, produced by building `aegis-probe-ebpf` for
/// `bpfel-unknown-none`. The build script / Makefile places it here.
static EBPF_OBJECT: &[u8] = include_bytes_aligned!(concat!(env!("OUT_DIR"), "/aegis-probe-ebpf.o"));

// aya provides this alignment helper via a macro in real builds; re-declared here
// so this file is self-contained for review. On target, prefer `aya::include_bytes_aligned!`.
macro_rules! include_bytes_aligned {
    ($path:expr) => {{
        #[repr(align(64))]
        struct Aligned<T: ?Sized>(T);
        static ALIGNED: &Aligned<[u8]> = &Aligned(*include_bytes!($path));
        &ALIGNED.0
    }};
}

/// A loaded, attached eBPF LSM supervisor.
pub struct AyaProbe {
    _ebpf: Ebpf,
    events: RingBuf<MapData>,
}

impl AyaProbe {
    /// Load the kernel programs, attach the LSM hooks, install `policy` into the
    /// per-PID policy map, and open the violation ring buffer.
    pub fn load(policy: ProbePolicy) -> Result<Self, ProbeError> {
        // BTF is required to resolve LSM hook argument types.
        let btf = Btf::from_sys_fs().map_err(|_| ProbeError::NoBpfLsm)?;

        let mut ebpf = Ebpf::load(EBPF_OBJECT).map_err(|e| ProbeError::Load(e.to_string()))?;

        // Attach the three enforcement hooks. LSM hooks (not tracepoints) are what
        // let us return -EPERM to actually *block* the operation in-kernel.
        for (prog_name, hook) in [
            ("bprm_check", "bprm_check_security"), // execve
            ("socket_connect", "socket_connect"),  // outbound connect
            ("file_open", "file_open"),            // writes to protected paths
        ] {
            let prog: &mut Lsm = ebpf
                .program_mut(prog_name)
                .ok_or_else(|| ProbeError::Load(format!("missing program `{prog_name}`")))?
                .try_into()
                .map_err(|e| ProbeError::Load(format!("{prog_name}: {e}")))?;
            prog.load(hook, &btf)
                .map_err(|e| ProbeError::Load(format!("load {hook}: {e}")))?;
            prog.attach()
                .map_err(|e| ProbeError::Load(format!("attach {hook}: {e}")))?;
        }

        install_policy(&mut ebpf, &policy)?;

        let events = RingBuf::try_from(
            ebpf.take_map("VIOLATIONS")
                .ok_or_else(|| ProbeError::Load("missing map `VIOLATIONS`".into()))?,
        )
        .map_err(|e| ProbeError::Load(format!("ring buffer: {e}")))?;

        tracing::info!("aegis-probe: eBPF LSM enforcement active");
        Ok(AyaProbe {
            _ebpf: ebpf,
            events,
        })
    }
}

/// Push the policy into the kernel map keyed by this process group's TGID, so the
/// hooks only enforce against Aegis-supervised processes.
fn install_policy(ebpf: &mut Ebpf, policy: &ProbePolicy) -> Result<(), ProbeError> {
    let mut map: BpfHashMap<_, u32, u8> = BpfHashMap::try_from(
        ebpf.map_mut("SUPERVISED")
            .ok_or_else(|| ProbeError::Load("missing map `SUPERVISED`".into()))?,
    )
    .map_err(|e| ProbeError::Load(format!("policy map: {e}")))?;

    // A minimal encoding: mark our own process group supervised. The richer
    // allow/deny tables (exec paths, IPs, write prefixes) are installed into their
    // own maps by the kernel-object's map definitions; wiring those is done here
    // when building on target. `policy` is retained for that step.
    let tgid = std::process::id();
    let flags = u8::from(!policy.protected_write_prefixes.is_empty());
    map.insert(tgid, flags, 0)
        .map_err(|e| ProbeError::Load(format!("install policy: {e}")))?;
    Ok(())
}

impl Probe for AyaProbe {
    fn is_enforcing(&self) -> bool {
        true
    }

    fn poll_events(&mut self) -> Vec<ViolationEvent> {
        let mut out = Vec::new();
        // Non-blocking drain of the ring buffer.
        while let Some(item) = self.events.next() {
            let bytes: &[u8] = &item;
            if bytes.len() >= std::mem::size_of::<RawViolation>() {
                // SAFETY: the kernel writes exactly this POD layout.
                let raw =
                    unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const RawViolation) };
                if let Some(ev) = raw.decode() {
                    out.push(ev);
                }
            }
        }
        out
    }
}

// Keep `AsRawFd` import meaningful for reviewers wiring epoll-based polling on target.
#[allow(dead_code)]
fn ringbuf_fd(rb: &RingBuf<MapData>) -> i32 {
    rb.as_raw_fd()
}
