# Source this file to select the same test graph with either local disk caching
# or the R2 proxy. Never infer credential availability from repo.fork: a PR
# within a fork repository may have credentials, while other runs may not.
export JOSH_CI_REMOTE_CACHE=false
export JOSH_CI_COMPOSE_FILTER=':+compose'
if [[ -n "${AWS_ACCESS_KEY_ID:-}" && -n "${AWS_SECRET_ACCESS_KEY:-}" ]]; then
    export JOSH_CI_REMOTE_CACHE=true
    export JOSH_CI_COMPOSE_FILTER=':[::sidecars.josh=ci-sidecars.josh,:/]:+compose'
fi
