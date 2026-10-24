# selx development tasks — `just` lists them.

default:
    @just --list

# Release build of the selx binary
build:
    cargo build --release

# The licence step checks every crate in Cargo.lock against deny.toml. It
# needs cargo-deny; CI installs the pinned version (see Jenkinsfile), and
# locally: `cargo install cargo-deny --locked --version 0.20.2`.

# What CI runs: fmt, clippy (warnings deny), workspace tests, dependency licences
test:
    cargo fmt --all -- --check
    cargo clippy --all-targets -- -D warnings
    cargo test
    cargo deny check licenses
    just layering

# The layering the modules promise (see src/lib.rs): core imports neither
# the adapters nor the command layer, and the adapters do not import the
# command layer. The compiler enforced this when they were three crates;
# this grep does now. grep exits 1 on no match, which is the pass.

# Check that no module imports a layer above it
layering:
    ! grep -rn -E 'crate::(adapters|cmd|config|env|out)\b' src/core
    ! grep -rn -E 'crate::(cmd|config|env|out)\b' src/adapters

# Deliberately not part of `test`, and deliberately not pinned: it needs
# the network to fetch the advisory database, a new advisory in a
# transitive crate is news rather than a broken build, and an advisory
# scanner is the one tool you always want the newest of. 116 crates in the
# lock file for a 3-crate workspace is reasonable; nothing else watches
# them, so run this when touching dependencies.

# Check the dependency tree for security advisories.
audit:
    command -v cargo-audit >/dev/null || cargo install cargo-audit --locked
    cargo audit

# The release tarball for this machine, in dist/ (what the release job runs
# on each agent).

# Build the release tarball for this machine into dist/
package:
    scripts/package.sh "$(rustc -vV | sed -n 's/^host: //p')"

# Install selx into ~/.cargo/bin
install:
    cargo install --path . --locked
    @echo "note: running daemons keep the old binary; an idle one restarts itself on the"
    @echo "      next selx command, one with an agent attached warns until 'selx stop'"
