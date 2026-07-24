#!/usr/bin/env bash
# Rename the project from Kedge to Kedge.
#
# Ordering matters. A blind `sed s/kedge/kedge/g` would corrupt things that only
# *look* like the project name, so replacements run most-specific first and each
# class of identifier is handled explicitly:
#
#   1. directories        crates/kedge-x     -> crates/kedge-x
#   2. crate names        kedge-x, sturdy-x  -> kedge-x        (Cargo.toml, deps)
#   3. rust module paths  aegis_x, sturdy_x  -> kedge_x        (use statements)
#   4. env var            KEDGE_LEDGER_PATH  -> KEDGE_LEDGER_PATH
#   5. ledger filename    kedge.sqlite       -> kedge.sqlite
#   6. repo URLs          SturdyRobot/kedge  -> SturdyRobot/kedge
#   7. prose              Kedge -> Kedge, kedge -> kedge
#
# MCP tool names (kedge_compact/kedge_audit/kedge_run) are a published API
# surface: renaming them breaks every existing .mcp.json. They are handled in
# step 3 along with module paths, so callers MUST update their config — that is
# a deliberate breaking change, appropriate pre-1.0.
#
# Usage:  scripts/rename-to-kedge.sh [--dry-run]
set -euo pipefail

DRY=0
[[ "${1:-}" == "--dry-run" ]] && DRY=1

cd "$(git rev-parse --show-toplevel)"

say() { printf '  %s\n' "$*"; }
run() { if [[ $DRY -eq 1 ]]; then say "[dry] $*"; else eval "$@"; fi; }

# ── 1. directories ──────────────────────────────────────────────────────────
say "renaming crate directories"
for d in crates/kedge-* crates/sturdy-*; do
  [[ -d "$d" ]] || continue
  new="crates/kedge-$(basename "$d" | sed -E 's/^(kedge|sturdy)-//')"
  [[ "$d" == "$new" ]] && continue
  run "git mv '$d' '$new'"
done

# Files to rewrite: tracked text only. Never touch target/, pkg/, or binaries.
mapfile -t FILES < <(git ls-files | grep -vE '^(target/|.*/pkg/)' | while read -r f; do
  [[ -f "$f" ]] && file --mime "$f" 2>/dev/null | grep -q 'charset=binary' || echo "$f"
done)
say "rewriting ${#FILES[@]} tracked text files"

# NOTE: this uses perl, not sed, deliberately. BSD/macOS sed does not support
# `\b` word boundaries — those expressions silently match nothing, which leaves
# the tree half-renamed and non-compiling. perl's regex is consistent on macOS
# and Linux.
subst() { # subst <perl-expr> <grep-pattern> <label>
  if [[ $DRY -eq 1 ]]; then
    n=$(grep -rlE "$2" "${FILES[@]}" 2>/dev/null | wc -l | tr -d ' ')
    say "[dry] $3 — would touch $n files"
  else
    printf '%s\0' "${FILES[@]}" | xargs -0 perl -pi -e "$1"
    say "$3"
  fi
}

# ── 2+3. crate names and module paths ───────────────────────────────────────
subst 's/\b(kedge|sturdy)-(core|ledger|mcp|exec|llm|compact|audit|bridge|cache|eval|hitl|mesh|policy|probe|server|web)\b/kedge-\2/g' \
      '\b(kedge|sturdy)-(core|ledger|mcp)' 'crate names -> kedge-*'
subst 's/\b(kedge|sturdy)_(core|ledger|mcp|exec|llm|compact|audit|bridge|cache|eval|hitl|mesh|policy|probe|server|web)\b/kedge_\2/g' \
      '\b(kedge|sturdy)_(core|ledger|mcp)' 'module paths -> kedge_*'
# probe-ebpf is a compound suffix the pattern above misses.
subst 's/kedge-probe-ebpf/kedge-probe-ebpf/g; s/kedge_probe_ebpf/kedge_probe_ebpf/g' \
      'kedge-probe-ebpf' 'probe-ebpf'
# MCP tool names — a deliberate breaking change to the published surface.
subst 's/\baegis_(compact|audit|run)\b/kedge_\1/g' '\baegis_(compact|audit|run)\b' 'MCP tool names'

# ── 3b. compound identifiers ────────────────────────────────────────────────
# Word-boundary rules correctly skip these because the name is glued to other
# words. Each is a real published surface, so each is renamed explicitly.
subst 's/kedge_delegate_task/kedge_delegate_task/g' 'kedge_delegate_task' 'delegate tool name'
subst 's/kedge_rt/kedge_rt/g'                       'kedge_rt'            'python module (kedge-bridge)'
subst 's/WasmKedgeAgent/WasmKedgeAgent/g'           'WasmKedgeAgent'      'wasm JS class'
subst 's/__kedgeDone/__kedgeDone/g'                 '__kedgeDone'         'wasm demo JS global'

# ── 4+5. env var and ledger filename ────────────────────────────────────────
subst 's/KEDGE_LEDGER_PATH/KEDGE_LEDGER_PATH/g' 'KEDGE_LEDGER_PATH' 'env var'
subst 's/kedge\.sqlite/kedge.sqlite/g' 'kedge\.sqlite' 'ledger filename'
subst 's|\.kedge/ledger\.sqlite|.kedge/ledger.sqlite|g' '\.kedge/ledger' 'central ledger path'

# ── 6. repo URLs (before generic prose, which would mangle them) ────────────
subst 's|SturdyRobot/kedge|SturdyRobot/kedge|g' 'SturdyRobot/kedge' 'repo URLs'

# ── 7. prose + the binary name ──────────────────────────────────────────────
subst 's/\bAegis\b/Kedge/g' '\bAegis\b' 'prose (Kedge -> Kedge)'
subst 's/\baegis\b/kedge/g' '\baegis\b' 'lowercase (kedge -> kedge)'

# ── 8. reformat ─────────────────────────────────────────────────────────────
# `kedge_core` is shorter than `kedge_core`, so line widths shift and rustfmt
# wants to reflow. CI gates on `cargo fmt --check`, so this is not optional.
if [[ $DRY -eq 1 ]]; then
  say "[dry] would run cargo fmt --all"
else
  say "running cargo fmt --all"
  cargo fmt --all || say "WARNING: cargo fmt failed — run it manually before committing"
fi

say "done. Verify with: cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace"
