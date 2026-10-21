# List the recipes.
[private]
default:
    @just --list --unsorted

# Run all CI jobs (except the smoke test: `devenv test`).
ci: fmt clippy test doc deny

# Check formatting.
fmt:
    cargo fmt --all --check

# Run clippy, on the host and on wasm32.
clippy:
    cargo clippy --workspace --all-targets --locked -- --deny warnings
    cargo clippy --workspace --all-targets --target wasm32-unknown-unknown --locked -- --deny warnings

# Run tests.
test:
    cargo test --workspace --locked

# Run rustdoc, on the host and on wasm32.
[env("RUSTDOCFLAGS", "--deny warnings")]
doc:
    cargo doc --no-deps --locked
    cargo doc --no-deps --target wasm32-unknown-unknown --locked

# Check dependencies (advisories, licenses, sources).
deny:
    cargo deny --locked check

# Smoke test a running smoke Worker (`devenv up wrangler-smoke`); `devenv test` does both.
smoke url="http://127.0.0.1:8787" timeout="300":
    #!/usr/bin/env bash
    set -euo pipefail

    # wrangler builds the Worker before it listens.
    for _ in $(seq 1 {{ timeout }}); do
        curl --silent --output /dev/null "{{ url }}/" && break
        sleep 1
    done

    failed=0
    for route in test-get test-post test-stream test-abort test-timeout test-timeout-ext test-timeout-with-signal test-timeout-ok test-redirect test-redirect-binding test-drop-future test-drop-body test-binding test-tower; do
        body="$(curl --silent --show-error --write-out '\n%{http_code}' "{{ url }}/${route}" || true)"
        status="${body##*$'\n'}"
        body="${body%$'\n'*}"
        if [[ "$status" == "200" ]] && grep -q '"ok":true' <<<"$body"; then
            echo "PASS /${route}  ${body}"
        else
            echo "FAIL /${route}  HTTP ${status}  ${body}"
            failed=1
        fi
    done
    exit "$failed"
