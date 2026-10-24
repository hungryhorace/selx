# Docker dogfood harness

An interactive amd64 container with **helix + lazygit + Claude Code +
herdr**, wired up with the shipped `integrations/` snippets, for
end-to-end dogfooding that never touches the host environment. The selx
binary is bind-mounted from the host's debug build — `cargo build` and
rerun is the whole edit cycle; the image has no Rust toolchain.

## One-time setup

```sh
docker/build.sh                 # stages host lazygit + snippets, builds image
claude setup-token              # on the host; then:
cp docker/.env.example docker/.env   # paste the token in
```

No token? `run.sh` falls back to mounting `~/.claude/.credentials.json`
read-only; the entrypoint copies it once into a container-local volume
(`selx-dogfood-claude`), so Claude's token refreshes never touch the host
file.

## Use

```sh
docker/run.sh                   # bash in the container
docker/run.sh herdr             # straight into herdr
```

Inside, `~/ws` is a throwaway git repo (with an uncommitted change for
lazygit's staging view). The entrypoint pre-seeds herdr with the dogfood
layout — a `ws` workspace on `~/ws` with `hx src/capture.rs` and `lazygit`
stacked on the left and `selx claude` on the right — so `docker/run.sh
herdr` lands straight in the loop. The dogfood loop:

1. `docker/run.sh herdr` — or from a bash container, just `herdr`
   (attaches to the same pre-seeded session).
2. Select lines in helix, hit `<space>T` — Claude's next prompt carries
   `@src/capture.rs#L..`; a non-Claude agent pane gets the reference typed
   into its input, unsubmitted. `<space>t` shares them quietly (ambient).
3. `lazygit`: `t` shares the selected file and `e` in the staging view
   shares the diff line under the cursor (both ambient); `T` sends the
   selected file explicitly.
4. `selx status --follow` in another pane shows every event and its routing.

Claude state (`CLAUDE_CONFIG_DIR=/home/dev/.claude-config`) lives in the
named volume; `docker volume rm selx-dogfood-claude` resets it, including
the copied credentials. Everything else is discarded on exit (`--rm`).

## Notes

- amd64 only — only the amd64 CI node has Docker (and lazygit is copied
  from the host binary).
- The image pins helix 25.07.1 (the version the snippet is verified
  against); herdr installs "latest" at build time. The Claude Code binary
  is staged by build.sh — copied from the host's own install when present
  (no bandwidth), else a resumable download — and cached in stage/;
  `rm docker/stage/claude` + rebuild to update it. The image sets
  DISABLE_AUTOUPDATER=1 so it stays at the staged version.
- `docker/stage/` and `docker/.env` are gitignored build artifacts.
