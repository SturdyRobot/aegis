#!/usr/bin/env bash
# Publish the remaining kedge crates, waiting out crates.io's new-crate rate
# limit (a burst of ~5, then roughly one per 10 minutes).
#
# Order is dependency-correct but front-loads whatever `kedge` needs, so the
# flagship name is claimed as early as the limit allows rather than last.
set -uo pipefail
cd "$(git rev-parse --show-toplevel)"

ORDER=(
  # --- required by `kedge`, so these come first ---
  kedge-mcp
  kedge-audit
  kedge-hitl
  kedge-eval
  kedge-server
  kedge          # flagship — claimed as soon as its deps exist
  # --- independent, no rush ---
  kedge-policy
  kedge-cache
  kedge-mesh
  kedge-probe
)

log() { printf '[%s] %s\n' "$(date -u '+%H:%M:%S')" "$*"; }

is_live() { # already on crates.io?
  curl -s --max-time 10 -H "User-Agent: kedge-publish (noeljacksonjs@gmail.com)" \
    "https://crates.io/api/v1/crates/$1" 2>/dev/null | grep -q '"crate"'
}

for crate in "${ORDER[@]}"; do
  if is_live "$crate"; then log "$crate already live — skipping"; continue; fi

  # Up to ~40 min of retries per crate; the limit is time-based, not per-token.
  for attempt in $(seq 1 20); do
    out=$(cargo publish -p "$crate" 2>&1)
    if grep -qE '^ *Published' <<<"$out"; then
      log "PUBLISHED $crate"
      break
    fi
    if grep -q '429 Too Many Requests' <<<"$out"; then
      log "$crate rate-limited (attempt $attempt) — sleeping 120s"
      sleep 120
      continue
    fi
    # Anything that isn't a rate limit is a real failure worth stopping on.
    log "FAILED $crate — not a rate limit:"
    tail -6 <<<"$out" | sed 's/^/    /'
    break
  done

  # crates.io needs a moment to index a new crate before a dependent can verify.
  sleep 15
done

log "done. Final state:"
for c in "${ORDER[@]}"; do
  is_live "$c" && log "  LIVE     $c" || log "  MISSING  $c"
done
