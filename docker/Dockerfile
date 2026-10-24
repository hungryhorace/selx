# selx dogfood container: helix + lazygit + Claude Code + herdr on
# ubuntu:24.04, so end-to-end testing never touches the host environment.
# amd64 only (matches the one Docker-capable CI node). Build with
# docker/build.sh — it stages the host's lazygit and the shipped
# integration snippets into docker/stage/ first.
#
# The selx binary is NOT baked in: docker/run.sh bind-mounts the host's
# debug build, so `cargo build` + rerun is the whole edit cycle.

FROM ubuntu:24.04

ARG HELIX_VERSION=25.07.1
ARG UID=1000
ARG GID=1000

RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        ca-certificates curl git xz-utils procps jq \
    && rm -rf /var/lib/apt/lists/*

# Helix from the GitHub release tarball (the host uses snap; snapd does not
# run in containers).
RUN curl -fsSL -o /tmp/helix.tar.xz \
        "https://github.com/helix-editor/helix/releases/download/${HELIX_VERSION}/helix-${HELIX_VERSION}-x86_64-linux.tar.xz" \
    && mkdir -p /opt/helix \
    && tar -xJf /tmp/helix.tar.xz -C /opt/helix --strip-components=1 \
    && ln -s /opt/helix/hx /usr/local/bin/hx \
    && ln -s /opt/helix/hx /usr/local/bin/helix \
    && rm /tmp/helix.tar.xz
ENV HELIX_RUNTIME=/opt/helix/runtime

# lazygit copied from the host (staged by build.sh; docker COPY cannot reach
# outside the build context).
COPY stage/lazygit /usr/local/bin/lazygit

# Non-root user matching the host uid/gid so bind mounts map cleanly.
# ubuntu:24.04 ships an 'ubuntu' user at 1000; replace it.
RUN userdel -r ubuntu 2>/dev/null || true \
    && groupadd -g "${GID}" dev 2>/dev/null || true \
    && useradd -m -u "${UID}" -g "${GID}" -s /bin/bash dev
USER dev
WORKDIR /home/dev
ENV PATH="/home/dev/.local/bin:${PATH}"

# herdr via its installer (installs under $HOME/.local/bin).
RUN curl -fsSL https://herdr.dev/install.sh | sh
# Claude Code binary staged by build.sh (downloaded host-side with resume
# + retries — a one-shot in-container download dies on flaky connections).
COPY --chown=dev:dev --chmod=755 stage/claude /home/dev/.local/bin/claude
# Keep the image's Claude at the staged version.
ENV DISABLE_AUTOUPDATER=1

# The shipped editor snippets, verbatim, as the user configs — dogfooding
# exactly what integrations/ ships.
COPY --chown=dev:dev stage/helix-config.toml /home/dev/.config/helix/config.toml
COPY --chown=dev:dev stage/lazygit-config.yml /home/dev/.config/lazygit/config.yml

# Claude state lives in a container path backed by a named volume (run.sh);
# never the host's ~/.claude — both selx and Claude write there. Pre-create
# it owned by dev: a named volume copies ownership from the image path on
# first use, and would otherwise be root-owned (entrypoint chmod fails).
RUN mkdir -m 700 /home/dev/.claude-config
ENV CLAUDE_CONFIG_DIR=/home/dev/.claude-config

COPY --chown=dev:dev --chmod=755 stage/entrypoint.sh /home/dev/entrypoint.sh
ENTRYPOINT ["/home/dev/entrypoint.sh"]
CMD ["bash"]
