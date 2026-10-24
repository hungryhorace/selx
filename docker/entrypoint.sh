#!/bin/bash
# selx dogfood container entrypoint: credentials + a sample workspace.
set -e

mkdir -p "$CLAUDE_CONFIG_DIR" 2>/dev/null || true
chmod 700 "$CLAUDE_CONFIG_DIR" 2>/dev/null || true
if [ ! -w "$CLAUDE_CONFIG_DIR" ]; then
    echo "entrypoint: ERROR: $CLAUDE_CONFIG_DIR is not writable (volume from" >&2
    echo "  an older image?). Fix with: docker volume rm selx-dogfood-claude" >&2
    exit 1
fi

# Credentials, in order of preference:
#  1. CLAUDE_CODE_OAUTH_TOKEN from docker/.env (via `claude setup-token`).
#  2. Host ~/.claude/.credentials.json mounted read-only; copied once into
#     the named volume so Claude can refresh it without touching the host.
if [ -z "${CLAUDE_CODE_OAUTH_TOKEN:-}" ]; then
    if [ -f /host-credentials.json ] && [ ! -f "$CLAUDE_CONFIG_DIR/.credentials.json" ]; then
        cp /host-credentials.json "$CLAUDE_CONFIG_DIR/.credentials.json"
        chmod 600 "$CLAUDE_CONFIG_DIR/.credentials.json"
        echo "entrypoint: copied host credentials into $CLAUDE_CONFIG_DIR (container-local from now on)"
    fi
    if [ ! -f "$CLAUDE_CONFIG_DIR/.credentials.json" ]; then
        echo "entrypoint: WARNING: no Claude credentials." >&2
        echo "  On the host: 'claude setup-token', put the token in docker/.env" >&2
        echo "  (CLAUDE_CODE_OAUTH_TOKEN=...), and rerun docker/run.sh." >&2
    fi
fi

# A throwaway git workspace to select lines in.
if [ ! -d "$HOME/ws/.git" ]; then
    mkdir -p "$HOME/ws/src"
    cd "$HOME/ws"
    git init -q
    git config user.email dev@selx-dogfood
    git config user.name "selx dogfood"
    seq -f 'fn line %g of capture.rs' 1 60 > src/capture.rs
    seq -f 'fn line %g of route.rs' 1 40 > src/route.rs
    git add -A && git commit -qm "sample workspace"
    echo "extra uncommitted line" >> src/route.rs   # something for lazygit's staging view
    cd "$HOME"
fi

if ! command -v selx >/dev/null; then
    echo "entrypoint: WARNING: selx not mounted — start via docker/run.sh" >&2
fi

# The pane id out of a `herdr pane split` reply on stdin.
pane_id() { jq -r '.result.pane.pane_id // empty'; }

# Pre-seed herdr with the dogfood layout — a "ws" workspace on ~/ws with
# hx + lazygit stacked on the left and `selx claude` on the right — so
# `docker/run.sh herdr` (or a manual `herdr` later) lands straight in the
# loop. The container is ephemeral (--rm), so this runs once per run.
seed_herdr() {
    # `herdr status server` exits 0 even with no server; probe the API instead.
    if ! herdr workspace list >/dev/null 2>&1; then
        herdr server >"$HOME/.herdr-server.log" 2>&1 &
        for _ in $(seq 1 50); do
            herdr workspace list >/dev/null 2>&1 && break
            sleep 0.1
        done
    fi
    # Only seed an empty session; never disturb a layout that is already
    # there. herdr always answers JSON, so `jq` reads it (a `sed` over the
    # raw reply was one key-order change away from silently seeding
    # nothing).
    herdr workspace list | jq -e '.result.workspaces | length == 0' >/dev/null || return 0
    # First workspace in an empty session: root pane is w1:p1.
    herdr workspace create --cwd "$HOME/ws" --label ws --focus >/dev/null
    agent_pane=$(herdr pane split w1:p1 --direction right | pane_id)
    git_pane=$(herdr pane split w1:p1 --direction down | pane_id)
    if [ -z "$agent_pane" ] || [ -z "$git_pane" ]; then
        echo "entrypoint: WARNING: herdr did not name the split panes" >&2
        return 1
    fi
    herdr pane run w1:p1 'hx src/capture.rs' >/dev/null
    herdr pane run "$git_pane" lazygit >/dev/null
    herdr pane run "$agent_pane" 'selx claude' >/dev/null
}
if command -v herdr >/dev/null; then
    seed_herdr || echo "entrypoint: WARNING: herdr layout seeding failed" >&2
fi

exec "$@"
