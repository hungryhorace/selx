#!/bin/bash
# Run the selx dogfood container interactively.
#   docker/run.sh            # bash inside the container
#   docker/run.sh herdr      # straight into herdr
set -euo pipefail
cd "$(dirname "$0")/.."

# Debug selx bind-mounted into the container (no toolchain in the image).
# Target dir via cargo metadata: CI nodes use a shared CARGO_TARGET_DIR.
cargo build -q
target_dir=$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"//; s/".*//p')
selx_bin="$target_dir/debug/selx"
[ -x "$selx_bin" ] || { echo "no binary at $selx_bin" >&2; exit 1; }

args=(
    --rm -it
    --name selx-dogfood
    --hostname selx-dogfood
    -v "$selx_bin":/usr/local/bin/selx:ro
    # Claude state persists (token refresh) in a container-local volume,
    # never the host's ~/.claude.
    -v selx-dogfood-claude:/home/dev/.claude-config
)

# Credentials: docker/.env (CLAUDE_CODE_OAUTH_TOKEN=... from
# `claude setup-token`) wins; else fall back to a read-only mount of the
# host credentials, copied container-side by the entrypoint.
if [ -f docker/.env ]; then
    args+=(--env-file docker/.env)
elif [ -f "$HOME/.claude/.credentials.json" ]; then
    args+=(-v "$HOME/.claude/.credentials.json":/host-credentials.json:ro)
fi

exec docker run "${args[@]}" selx-dogfood "$@"
