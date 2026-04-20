# syntax=docker/dockerfile:1.7
#
# Multi-stage build for `smiths-net`. Produces a ~20 MB scratch-based
# image carrying a statically-linked `smiths-net` binary. Intended to
# be built with `docker buildx build --platform linux/amd64,linux/arm64`
# so one manifest covers both production targets.
#
# The release workflow (.github/workflows/release.yml) calls buildx on
# tagged commits and pushes to the configured registry.

# -- builder -----------------------------------------------------------
# Pin the toolchain image. `clux/muslrust` provides a musl-gcc setup that
# matches what `rust-toolchain` would install locally, plus a working
# `pkg-config` for ring. Using the explicit rust version keeps nightly
# churn out of the release chain.
FROM --platform=$BUILDPLATFORM clux/muslrust:1.85.0-stable AS builder

# TARGETARCH is set by buildx (amd64, arm64). Map to the Rust target
# triple so the same Dockerfile builds for both.
ARG TARGETARCH
RUN case "${TARGETARCH}" in \
      amd64) echo "x86_64-unknown-linux-musl" > /tmp/rust-target ;; \
      arm64) echo "aarch64-unknown-linux-musl" > /tmp/rust-target ;; \
      *) echo "unsupported TARGETARCH: ${TARGETARCH}" >&2 ; exit 1 ;; \
    esac && rustup target add "$(cat /tmp/rust-target)"

WORKDIR /src

# Copy manifests first so `cargo fetch` caches across source-only
# rebuilds. The full source follows once the dep resolution is warm.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY plugins ./plugins
COPY proto ./proto
COPY examples ./examples

# Build the CLI binary only — testkit / fuzz / plugin SDKs are not
# needed in the production image.
RUN --mount=type=cache,target=/src/target \
    --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release \
        --target "$(cat /tmp/rust-target)" \
        --bin smiths-net && \
    cp "target/$(cat /tmp/rust-target)/release/smiths-net" /tmp/smiths-net

# -- runtime -----------------------------------------------------------
# Distroless-static — glibc-free, no shell, no package manager. Matches
# the musl-static binary's zero-libc requirement.
FROM gcr.io/distroless/static-debian12:nonroot AS runtime

# Bundled default config — operators are expected to bind-mount their
# own over /etc/smiths-net/config.toml.
COPY --from=builder /src/examples/config.toml /etc/smiths-net/config.toml.example
COPY --from=builder /tmp/smiths-net /usr/local/bin/smiths-net

# Default ports:
#   5060/udp  SIP UDP transport
#   5060/tcp  SIP TCP transport
#   5061/tcp  SIP TLS transport
#   8080/tcp  health + /metrics
EXPOSE 5060/udp 5060/tcp 5061/tcp 8080/tcp

# `nonroot` is the default user; distroless sets it implicitly.
ENTRYPOINT ["/usr/local/bin/smiths-net"]
CMD ["--config", "/etc/smiths-net/config.toml"]
