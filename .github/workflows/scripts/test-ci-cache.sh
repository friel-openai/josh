#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../../.."
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
unset AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY
source .github/workflows/scripts/ci-cache-config.sh
[[ "$JOSH_CI_REMOTE_CACHE" == false && "$JOSH_CI_COMPOSE_FILTER" == ':+compose' ]]
export AWS_ACCESS_KEY_ID=test-key
source .github/workflows/scripts/ci-cache-config.sh
[[ "$JOSH_CI_REMOTE_CACHE" == false ]]
unset AWS_ACCESS_KEY_ID
export AWS_SECRET_ACCESS_KEY=test-secret
source .github/workflows/scripts/ci-cache-config.sh
[[ "$JOSH_CI_REMOTE_CACHE" == false ]]
export AWS_ACCESS_KEY_ID=test-key
source .github/workflows/scripts/ci-cache-config.sh
[[ "$JOSH_CI_REMOTE_CACHE" == true && "$JOSH_CI_COMPOSE_FILTER" == ':[::sidecars.josh=ci-sidecars.josh,:/]:+compose' ]]
cat > "$work/josh" <<'MOCK'
#!/usr/bin/env bash
[[ "$#" == 4 && "$1" == compose && "$2" == run && "$3" == HEAD && "$4" == "$JOSH_CI_COMPOSE_FILTER" ]] || exit 99
echo "${TEST_CACHE_MESSAGE:-}"
exit "${TEST_JOSH_EXIT:-0}"
MOCK
chmod +x "$work/josh"
export TEST_CACHE_MESSAGE=SCCACHE_REMOTE_CACHE_MISSING
if bash .github/workflows/scripts/run-ci-tests.sh "$work/josh" > "$work/expected-failure.log" 2>&1; then
    echo "remote cache fallback must fail" >&2; exit 1
fi
export TEST_CACHE_MESSAGE='sccache: confirmed S3-backed cache'
bash .github/workflows/scripts/run-ci-tests.sh "$work/josh"
unset AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY
source .github/workflows/scripts/ci-cache-config.sh
export TEST_CACHE_MESSAGE=SCCACHE_REMOTE_CACHE_MISSING
bash .github/workflows/scripts/run-ci-tests.sh "$work/josh"
for backend in true false; do
    export JOSH_CI_REMOTE_CACHE="$backend" TEST_JOSH_EXIT=42
    export TEST_CACHE_MESSAGE='test compilation failed'
    status=0
    bash .github/workflows/scripts/run-ci-tests.sh "$work/josh" || status=$?
    [[ "$status" == 42 ]]
done
echo "CI cache selection and failure propagation passed"
