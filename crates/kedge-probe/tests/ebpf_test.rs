//! Probe integration tests.
//!
//! The portable test runs everywhere and pins the cross-platform contract. The
//! real kernel-enforcement test is gated to `--features ebpf` on Linux and marked
//! `#[ignore]` because it needs a BPF-LSM kernel + root — run it explicitly on a
//! target host: `sudo -E cargo test -p kedge-probe --features ebpf -- --ignored`.

use kedge_probe::{activate, ProbePolicy};

#[test]
fn portable_contract_uses_null_probe_without_ebpf() {
    // Without the `ebpf` feature (the default, and always on macOS/Windows), the
    // supervisor loads but enforces nothing — callers can wire it unconditionally.
    let probe = activate(ProbePolicy::hardened()).unwrap();
    assert!(!probe.is_enforcing());
}

/// Blocking test: a supervised process group must not be able to `execve` a binary
/// outside its allow-list; the attempt is denied in-kernel and journaled.
///
/// Requires: Linux ≥5.7 with `CONFIG_BPF_LSM` + `lsm=...,bpf`, root, and the
/// `kedge-probe-ebpf` object built (see BUILD_EBPF.md). Never runs in default CI.
#[cfg(all(target_os = "linux", feature = "ebpf"))]
#[test]
#[ignore = "requires a BPF-LSM Linux kernel and root"]
fn unauthorized_execve_is_blocked_and_logged() {
    use kedge_ledger::{Event, Ledger};
    use std::process::Command;

    // Allow only `/bin/true`; everything else should be blocked for this group.
    let policy = ProbePolicy {
        allowed_exec: vec!["/bin/true".into()],
        ..ProbePolicy::hardened()
    };
    let mut probe = activate(policy).expect("load eBPF probe (need root + BPF-LSM)");
    assert!(probe.is_enforcing());

    let ledger = Ledger::in_memory().unwrap();
    let run_id = kedge_core::TaskId::new();

    // An allowed exec succeeds…
    assert!(Command::new("/bin/true").status().unwrap().success());
    // …a disallowed one is denied by the kernel (non-zero / permission error).
    let denied = Command::new("/bin/sh").arg("-c").arg("echo pwned").status();
    assert!(
        denied.map(|s| !s.success()).unwrap_or(true),
        "the LSM hook should have blocked /bin/sh"
    );

    // The violation surfaced on the ring buffer and can be journaled.
    let events = probe.poll_events();
    let exec_violation = events
        .iter()
        .find(|e| matches!(e.kind, kedge_probe::ViolationKind::Exec))
        .expect("expected an exec violation event");
    kedge_probe::record_violation(&ledger, run_id, exec_violation).unwrap();

    let journaled = ledger.events(run_id).unwrap();
    assert!(journaled.iter().any(
        |e| matches!(e, Event::KernelSecurityViolation { boundary, .. } if boundary == "exec")
    ));
}
