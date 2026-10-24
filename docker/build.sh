#!/bin/bash
# Build the selx dogfood image. Stages host artifacts the Dockerfile cannot
# reach (docker COPY is limited to the build context).
set -euo pipefail
cd "$(dirname "$0")"

mkdir -p stage
cp /usr/local/bin/lazygit stage/lazygit
cp ../integrations/helix/config.toml.snippet stage/helix-config.toml
cp ../integrations/lazygit/config.yml.snippet stage/lazygit-config.yml
cp entrypoint.sh stage/entrypoint.sh

# Claude Code binary, cached in stage/ across builds (delete stage/claude
# to refresh). Prefer copying the host's own install — it is the same
# standalone linux-x64 ELF and costs no bandwidth; else a resumable
# download (a one-shot in-container download dies on flaky connections).
if [ ! -f stage/claude ]; then
    host_claude=$(readlink -f "$(command -v claude || true)" 2>/dev/null || true)
    if [ -n "$host_claude" ] && file -b "$host_claude" | grep -q 'ELF 64-bit.*x86-64'; then
        echo "staging host Claude Code: $host_claude"
        cp "$host_claude" stage/claude
    else
        ver=$(curl -fsSL --retry 5 https://downloads.claude.ai/claude-code-releases/latest)
        echo "downloading Claude Code $ver (resumable; safe to rerun build.sh if it dies)"
        for _ in $(seq 1 60); do
            curl -fL -C - --retry 10 --retry-delay 3 -o stage/claude.part \
                "https://downloads.claude.ai/claude-code-releases/$ver/linux-x64/claude" \
                && break
            sleep 3
        done
        [ -s stage/claude.part ] || { echo "claude download failed" >&2; exit 1; }
        mv stage/claude.part stage/claude
    fi
fi

docker build \
    --build-arg UID="$(id -u)" \
    --build-arg GID="$(id -g)" \
    -t selx-dogfood \
    "$@" \
    .
