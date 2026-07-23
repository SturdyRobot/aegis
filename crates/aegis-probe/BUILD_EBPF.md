# Building & running the eBPF enforcer

The portable half of `aegis-probe` (policy model, `NullProbe`, ledger events)
builds and tests everywhere with no extra setup — it's a normal workspace crate.

The **kernel enforcement** half is Linux-only and needs a real toolchain and a
capable kernel. It is deliberately **excluded from the workspace** and **off by
default**, so `cargo build/test --workspace` and CI stay kernel-free and
cross-platform.

> **Status:** the kernel object (`aegis-probe-ebpf`) and the Linux loader
> (`src/linux.rs`) are written but **were authored on macOS and have not been
> compiled or loaded**. Treat them as a reviewed starting point to validate on a
> target host, not as verified binaries.

## Host requirements

- Linux **≥ 5.7** built with `CONFIG_BPF_LSM=y`, and BPF enabled in the active
  LSM list: add `lsm=...,bpf` (or `bpf`) to the kernel cmdline and reboot. Check:
  ```sh
  cat /sys/kernel/security/lsm     # must contain "bpf"
  ```
- Root or `CAP_BPF` + `CAP_SYS_ADMIN` to load LSM programs.
- Rust **nightly** with `rust-src` (pinned by `aegis-probe-ebpf/rust-toolchain.toml`)
  and `bpf-linker`:
  ```sh
  rustup toolchain install nightly --component rust-src
  cargo install bpf-linker
  ```

## Build the kernel object

```sh
cd crates/aegis-probe-ebpf
cargo build --release          # → target/bpfel-unknown-none/release/aegis-probe-ebpf
```

`src/linux.rs` expects the object at `$OUT_DIR/aegis-probe-ebpf.o`; a small
`build.rs` (or `cargo xtask`) copies it there on target — wire this to your build.
Pin `aya`/`aya-ebpf`/`aya-log` to matching patch versions.

## Build & test the user-space enforcer

```sh
# From the repo root, on the Linux target:
sudo -E cargo test -p aegis-probe --features ebpf -- --ignored
```

`unauthorized_execve_is_blocked_and_logged` loads the probe, marks the test's
process group supervised, and asserts a disallowed `execve` is denied in-kernel
and journaled as `Event::KernelSecurityViolation`.

## Design notes

- Enforcement uses **LSM hooks** (`bprm_check_security`, `socket_connect`,
  `file_open`) — not tracepoints. A tracepoint can only observe; an LSM hook's
  return value is honored, so returning a negative errno (`-EPERM`) blocks.
- The hooks are **inert for unsupervised processes**: they early-return `0` unless
  the current TGID is in the `SUPERVISED` map, so the rest of the host is
  untouched.
- Bring-up defaults to **observe-and-report** (emit a `RingBuf` event, allow) so a
  supervised agent can't be bricked while the allow/deny maps are being populated;
  flip the `Ok(0)` paths to `Err(EPERM)` to enforce once path/addr resolution and
  the allow-maps are wired.
- The user-space and kernel `RawViolation` layouts must stay byte-identical;
  factor them into a shared `no_std` `-common` crate when you productionize.
