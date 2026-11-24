#!/usr/bin/env bash
set -euo pipefail

case "${JOSH_CI_REMOTE_CACHE:?cache backend must be selected}" in
    true|false) ;;
    *) echo "invalid CI cache backend" >&2; exit 1 ;;
esac
: "${JOSH_CI_COMPOSE_FILTER:?compose filter must be selected}"
log=$(mktemp)
trap 'rm -f "$log"' EXIT
"${1:-target/release/josh}" compose run HEAD "$JOSH_CI_COMPOSE_FILTER" 2>&1 | tee "$log"
# Credential-free runs intentionally use the image's SCCACHE_DIR disk cache.
# When R2 was selected, a silent fallback still indicates a broken CI setup.
if [[ "$JOSH_CI_REMOTE_CACHE" == true ]] && grep -q SCCACHE_REMOTE_CACHE_MISSING "$log"; then
    echo "::error::sccache remote cache was not used during the build" >&2
    exit 1
fi
