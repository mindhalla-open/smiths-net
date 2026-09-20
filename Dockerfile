# syntax=docker/dockerfile:1.7
#
# Multi-stage build for `smiths-net`: one statically linked (musl)
# binary on a distroless-static base, roughly 25 MB per architecture.
#
#   docker buildx build --platform linux/amd64,linux/arm64 -t smiths-net .
#
# The release workflow (.github/workflows/release.yml) builds exactly
# that on `v*` tags and pushes a single multi-arch manifest; the ci
# workflow builds the linux/amd64 half on every push / PR without
# pushing, so a Dockerfile regression fails a PR instead of a release.
#
# The builder stage runs on the *build* platform and cross-compiles for
# the *target* platform, so producing the arm64 image on an amd64
# runner does not go through QEMU-emulated rustc. The C dependencies
# of the binary (aws-lc-sys via reqwest's rustls backend, ring via
# webrtc-dtls, the bundled sqlite in rusqlite) are compiled with
# `zig cc`, a self-contained cross C compiler for both musl targets:
# Alpine ships no aarch64 cross gcc and a glibc cross gcc cannot
# target musl.

# -- builder -----------------------------------------------------------
# The Rust version matches the workspace `rust-version`; the Alpine
# minor is pinned as well so `apk add zig` yields the same compiler on
# every build. `rust-toolchain.toml` is deliberately not copied into
# the build context, so rustup uses the image's toolchain instead of
# fetching `stable`.
FROM --platform=$BUILDPLATFORM rust:1.95-alpine3.22 AS builder

RUN apk add --no-cache build-base cmake file pkgconf zig

# cargo-zigbuild points cc / cmake / rustc at `zig cc` for the requested
# target (compiler, archiver and linker), so a plain cargo build becomes
# a working cross build.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo install --locked cargo-zigbuild@0.23.4

# TARGETARCH is set by buildx (amd64, arm64). Map it to the Rust target
# triple so the same Dockerfile builds for both.
ARG TARGETARCH
RUN case "${TARGETARCH}" in \
      amd64) echo "x86_64-unknown-linux-musl" > /tmp/rust-target ;; \
      arm64) echo "aarch64-unknown-linux-musl" > /tmp/rust-target ;; \
      *) echo "unsupported TARGETARCH: ${TARGETARCH}" >&2 ; exit 1 ;; \
    esac && rustup target add "$(cat /tmp/rust-target)"

WORKDIR /src

# Only what the `smiths-net` binary needs: the workspace manifests, the
# crates, and the default config the runtime image ships.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY examples/config.toml ./examples/config.toml

# `--locked` refuses to build when Cargo.lock is out of date with the
# manifests, so a release image never silently re-resolves deps. The
# cargo target dir is a cache mount (never part of a layer), one per
# target architecture so a multi-platform build does not serialize on
# a shared lock; the one artifact we keep is copied out inside the
# same RUN. The final `file` check guards the distroless-static
# runtime, which has no libc for a dynamically linked binary to load.
RUN --mount=type=cache,id=smiths-net-target-${TARGETARCH},target=/src/target,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/registry \
    cargo zigbuild --release --locked \
        --target "$(cat /tmp/rust-target)" \
        --bin smiths-net \
 && cp "target/$(cat /tmp/rust-target)/release/smiths-net" /tmp/smiths-net \
 && file /tmp/smiths-net \
 && file /tmp/smiths-net | grep -q 'statically linked'

# -- export ------------------------------------------------------------
# `docker buildx build --target export --output type=local,dest=dist`
# writes just the binary per platform (dist/linux_amd64/smiths-net,
# dist/linux_arm64/smiths-net). The release workflow uses it to publish
# the very bytes the image carries as standalone release assets.
FROM scratch AS export
COPY --from=builder /tmp/smiths-net /smiths-net

# -- runtime -----------------------------------------------------------
# distroless-static: no libc, no shell, no package manager; runs as the
# `nonroot` user (uid 65532). The musl binary needs nothing from it.
FROM gcr.io/distroless/static-debian12:nonroot AS runtime

# Bundled default config so the image starts as-is. Operators
# bind-mount their own over /etc/smiths-net/config.toml (k8s/ does
# this from a ConfigMap) or override single keys through the
# environment, e.g. SMITHS__OBSERVABILITY__HEALTH_BIND=0.0.0.0:8080 to
# reach /health from outside the container (the default binds it to
# 127.0.0.1).
COPY --from=builder /src/examples/config.toml /etc/smiths-net/config.toml
COPY --from=builder /tmp/smiths-net /usr/local/bin/smiths-net

# Default config path for every invocation, so subcommands work
# without repeating it: `docker run --rm smiths-net validate`.
ENV SMITHS_CONFIG=/etc/smiths-net/config.toml

# Default ports:
#   5060/udp  SIP UDP transport
#   5060/tcp  SIP TCP transport
#   5061/tcp  SIP TLS transport (only once `sip.transports` includes tls)
#   8080/tcp  /health + /metrics
EXPOSE 5060/udp 5060/tcp 5061/tcp 8080/tcp

ENTRYPOINT ["/usr/local/bin/smiths-net"]
CMD []
