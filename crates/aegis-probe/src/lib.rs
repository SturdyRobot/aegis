//! # aegis-probe
//!
//! Kernel-boundary process supervision for Aegis. An AI agent that gets
//! prompt-injected can try to spawn subshells, phone home, or scribble on system
//! files — and user-space logging can be bypassed. This crate enforces hard
//! boundaries *below* user space, at the syscall/LSM layer.
//!
//! ## Portability
//!
//! The **policy model** and the **event/ledger plumbing** here are pure, portable
//! Rust — they build and are tested on every platform. The actual enforcement is
//! Linux eBPF **LSM** (not tracepoints — a tracepoint can observe but cannot
//! return `-EPERM`), lives behind the off-by-default `ebpf` feature, and is
//! `#[cfg(target_os = "linux")]`. Everywhere else — macOS, Windows, or Linux
//! without the feature/privileges — [`activate`] returns a [`NullProbe`] that
//! enforces nothing, so callers can wire supervision in unconditionally.
//!
//! The kernel programs themselves are the separate, workspace-**excluded**
//! `aegis-probe-ebpf` crate (it needs a nightly `bpfel-unknown-none` toolchain +
//! `bpf-linker`). See `BUILD_EBPF.md`.

use std::net::IpAddr;

use thiserror::Error;

#[cfg(all(target_os = "linux", feature = "ebpf"))]
mod linux;

/// Errors from activating or driving the probe.
#[derive(Debug, Error)]
pub enum ProbeError {
    #[error("eBPF enforcement is not available on this platform/build")]
    Unsupported,
    #[error("insufficient privileges to load eBPF programs (need root/CAP_BPF)")]
    Privileges,
    #[error("kernel does not support BPF LSM (need CONFIG_BPF_LSM and `lsm=...,bpf`)")]
    NoBpfLsm,
    #[error("eBPF load/attach failed: {0}")]
    Load(String),
    #[error("ledger error: {0}")]
    Ledger(#[from] sturdy_ledger::LedgerError),
}

// ── policy model (portable, the source of truth the kernel maps mirror) ──

/// Which kernel boundary a violation crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ViolationKind {
    /// A blocked `execve` (unpermitted binary).
    Exec,
    /// A blocked outbound `connect` (destination not on the allowlist).
    Connect,
    /// A blocked write to a protected path.
    Write,
}

impl ViolationKind {
    /// Stable lowercase label used in the ledger.
    pub fn as_str(self) -> &'static str {
        match self {
            ViolationKind::Exec => "exec",
            ViolationKind::Connect => "connect",
            ViolationKind::Write => "write",
        }
    }
}

/// The enforcement policy for a supervised process group. An empty allowlist for
/// exec/connect means "allow all" (observe-only for that boundary); the protected
/// write prefixes are always deny-on-match.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ProbePolicy {
    /// Absolute paths the agent may `execve`. Empty = allow all.
    pub allowed_exec: Vec<String>,
    /// Path prefixes that must never be written (e.g. `/etc/`, `/boot/`, `/usr/`).
    pub protected_write_prefixes: Vec<String>,
    /// Destination IPs the agent may `connect` to. Empty = allow all.
    pub allowed_connect: Vec<IpAddr>,
}

impl ProbePolicy {
    /// A sensible default deny-list for the write boundary: core system dirs.
    pub fn hardened() -> Self {
        ProbePolicy {
            allowed_exec: Vec::new(),
            protected_write_prefixes: ["/etc/", "/boot/", "/usr/", "/bin/", "/sbin/", "/lib/"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            allowed_connect: Vec::new(),
        }
    }

    /// Whether `path` is permitted to be executed.
    pub fn allows_exec(&self, path: &str) -> bool {
        self.allowed_exec.is_empty() || self.allowed_exec.iter().any(|a| a == path)
    }

    /// Whether writing `path` is permitted (denied if under any protected prefix).
    pub fn allows_write(&self, path: &str) -> bool {
        !self
            .protected_write_prefixes
            .iter()
            .any(|p| path.starts_with(p.as_str()))
    }

    /// Whether an outbound connection to `ip` is permitted.
    pub fn allows_connect(&self, ip: IpAddr) -> bool {
        self.allowed_connect.is_empty() || self.allowed_connect.contains(&ip)
    }
}

// ── violation events (what the kernel ring buffer surfaces to user space) ──

/// A single enforcement decision reported from the kernel. This is the decoded,
/// user-space form of the eBPF `RingBuf` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViolationEvent {
    pub pid: u32,
    pub tgid: u32,
    pub kind: ViolationKind,
    /// What was attempted: the exec path, destination address, or file path.
    pub detail: String,
}

/// Fixed-size record the eBPF program writes to the `RingBuf`. Kept `#[repr(C)]`
/// and POD so it can be read straight out of kernel memory. The companion
/// `aegis-probe-ebpf` crate writes the identical layout (ideally both would share
/// a `-common` crate; duplicated here to keep that crate build-isolated).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RawViolation {
    pub pid: u32,
    pub tgid: u32,
    pub kind: u8, // 0 = exec, 1 = connect, 2 = write
    pub detail_len: u16,
    pub detail: [u8; 256],
}

impl RawViolation {
    /// Decode a kernel record into a user-space [`ViolationEvent`].
    pub fn decode(&self) -> Option<ViolationEvent> {
        let kind = match self.kind {
            0 => ViolationKind::Exec,
            1 => ViolationKind::Connect,
            2 => ViolationKind::Write,
            _ => return None,
        };
        let len = (self.detail_len as usize).min(self.detail.len());
        let detail = String::from_utf8_lossy(&self.detail[..len]).into_owned();
        Some(ViolationEvent {
            pid: self.pid,
            tgid: self.tgid,
            kind,
            detail,
        })
    }
}

// ── the probe interface + null fallback ──

/// A live kernel supervisor. Dropping it detaches the eBPF programs.
pub trait Probe: Send {
    /// Whether kernel enforcement is actually active. `false` for [`NullProbe`].
    fn is_enforcing(&self) -> bool;
    /// Drain violation events observed since the last poll (never blocks).
    fn poll_events(&mut self) -> Vec<ViolationEvent>;
}

/// The portable no-op supervisor used off Linux, without the `ebpf` feature, or
/// wherever eBPF can't be loaded. It enforces nothing and reports nothing, so an
/// agent runtime can always call [`activate`] and carry on.
#[derive(Debug, Default)]
pub struct NullProbe;

impl Probe for NullProbe {
    fn is_enforcing(&self) -> bool {
        false
    }
    fn poll_events(&mut self) -> Vec<ViolationEvent> {
        Vec::new()
    }
}

/// Activate kernel supervision for `policy` over the calling process group.
///
/// Returns a real eBPF LSM probe on Linux built with `--features ebpf` and
/// sufficient privileges; otherwise a [`NullProbe`]. Callers should check
/// [`Probe::is_enforcing`] if they need to *know* whether the boundary is real.
pub fn activate(policy: ProbePolicy) -> Result<Box<dyn Probe>, ProbeError> {
    #[cfg(all(target_os = "linux", feature = "ebpf"))]
    {
        let probe = linux::AyaProbe::load(policy)?;
        return Ok(Box::new(probe));
    }
    #[cfg(not(all(target_os = "linux", feature = "ebpf")))]
    {
        let _ = policy;
        tracing::info!("aegis-probe: kernel enforcement unavailable; using NullProbe (no-op)");
        Ok(Box::new(NullProbe))
    }
}

/// Journal a kernel violation to the ledger as [`Event::KernelSecurityViolation`].
pub fn record_violation(
    ledger: &sturdy_ledger::Ledger,
    run_id: sturdy_core::TaskId,
    event: &ViolationEvent,
) -> Result<(), sturdy_ledger::LedgerError> {
    ledger.record_event(
        run_id,
        &sturdy_ledger::Event::KernelSecurityViolation {
            pid: event.pid,
            tgid: event.tgid,
            boundary: event.kind.as_str().to_string(),
            detail: event.detail.clone(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn exec_allowlist_semantics() {
        let mut p = ProbePolicy::default();
        assert!(p.allows_exec("/usr/bin/anything")); // empty allowlist ⇒ allow all
        p.allowed_exec.push("/bin/cargo".into());
        assert!(p.allows_exec("/bin/cargo"));
        assert!(!p.allows_exec("/bin/sh"));
    }

    #[test]
    fn protected_write_prefixes_block() {
        let p = ProbePolicy::hardened();
        assert!(!p.allows_write("/etc/passwd"));
        assert!(!p.allows_write("/usr/bin/aegis"));
        assert!(p.allows_write("/home/agent/workspace/out.txt"));
        assert!(p.allows_write("/tmp/scratch"));
    }

    #[test]
    fn connect_allowlist_semantics() {
        let mut p = ProbePolicy::default();
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5));
        assert!(p.allows_connect(ip)); // empty ⇒ allow all
        p.allowed_connect
            .push(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
        assert!(p.allows_connect(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(!p.allows_connect(ip));
    }

    #[test]
    fn null_probe_is_inert() {
        let mut np = NullProbe;
        assert!(!np.is_enforcing());
        assert!(np.poll_events().is_empty());
    }

    #[test]
    fn activate_falls_back_to_null_off_linux() {
        // On this build (no `ebpf` feature) activate must yield a non-enforcing probe.
        let probe = activate(ProbePolicy::hardened()).unwrap();
        assert!(!probe.is_enforcing());
    }

    #[test]
    fn raw_violation_round_trips() {
        let mut detail = [0u8; 256];
        let msg = b"/bin/sh";
        detail[..msg.len()].copy_from_slice(msg);
        let raw = RawViolation {
            pid: 1234,
            tgid: 1200,
            kind: 0,
            detail_len: msg.len() as u16,
            detail,
        };
        let ev = raw.decode().unwrap();
        assert_eq!(ev.kind, ViolationKind::Exec);
        assert_eq!(ev.detail, "/bin/sh");
        assert_eq!(ev.pid, 1234);
    }

    #[test]
    fn violation_is_journaled_to_ledger() {
        let ledger = sturdy_ledger::Ledger::in_memory().unwrap();
        let run_id = sturdy_core::TaskId::new();
        let ev = ViolationEvent {
            pid: 42,
            tgid: 40,
            kind: ViolationKind::Write,
            detail: "/etc/shadow".into(),
        };
        record_violation(&ledger, run_id, &ev).unwrap();
        let events = ledger.events(run_id).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            sturdy_ledger::Event::KernelSecurityViolation {
                boundary, detail, ..
            } => {
                assert_eq!(boundary, "write");
                assert_eq!(detail, "/etc/shadow");
            }
            _ => panic!("wrong event kind"),
        }
    }
}
