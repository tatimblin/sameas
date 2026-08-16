#!/bin/bash
# Builds the Rust worker to WASM via worker-build. Referenced by wrangler.toml's
# `[build] command`, so it runs on every `wrangler dev` and `wrangler deploy`
# locally as well as in Cloudflare's build pipeline (which is why it bootstraps
# rustup when cargo is missing).
set -e

if ! hash cargo 2>/dev/null; then
    echo "cargo not installed. We're probably in CI, so let's fix that now"
    curl https://sh.rustup.rs -sSf | sh -s -- -y
    . "$HOME/.cargo/env"
fi

# $WORKER_BUILD_FEATURES is set ONLY by vitest.config.mts (to
# `--features test-endpoints`, enabling the /__conformance route). It is unset for
# `wrangler dev` and `wrangler deploy`, so production never carries that route.
# Unquoted on purpose: it must word-split into two arguments, and expand to
# nothing when unset.
# shellcheck disable=SC2086
cargo install -q worker-build@0.1.1 && worker-build --release ${WORKER_BUILD_FEATURES}
