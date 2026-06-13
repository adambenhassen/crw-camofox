# Cross-compiling multi-arch build.
#
# The builder runs on the NATIVE build platform (`--platform=$BUILDPLATFORM`)
# and cross-compiles to the requested target arch. Previously the builder ran
# under QEMU for linux/arm64, which emulated the *entire* Rust compile and took
# ~2h per release. Cross-compiling on the native runner brings arm64 back to
# minutes; only the tiny runtime layer (ca-certificates) still touches QEMU.
FROM --platform=$BUILDPLATFORM rust:1.93-bookworm AS builder

# Provided automatically by buildx: amd64 | arm64. BUILDARCH is the native arch
# of the build host, so `$TARGETARCH != $BUILDARCH` ⇒ we are cross-compiling and
# need the target's cross toolchain (the native rustc image only ships its own).
ARG TARGETARCH
ARG BUILDARCH

WORKDIR /app

# Install the Rust target + (when cross-compiling) the target cross toolchain,
# and record the rustc target triple for the build step. crossbuild-essential-*
# = the target gcc/g++ AND the target libc dev headers (the bare cross gcc alone
# lacks sys/types.h etc., which broke aws-lc-sys's C build).
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) RUST_TARGET=x86_64-unknown-linux-gnu; \
             if [ "$BUILDARCH" != "amd64" ]; then \
               apt-get update; \
               apt-get install -y --no-install-recommends crossbuild-essential-amd64; \
               rm -rf /var/lib/apt/lists/*; \
             fi ;; \
      arm64) RUST_TARGET=aarch64-unknown-linux-gnu; \
             if [ "$BUILDARCH" != "arm64" ]; then \
               apt-get update; \
               apt-get install -y --no-install-recommends crossbuild-essential-arm64; \
               rm -rf /var/lib/apt/lists/*; \
             fi ;; \
      *) echo "unsupported TARGETARCH=$TARGETARCH" >&2; exit 1 ;; \
    esac; \
    rustup target add "$RUST_TARGET"; \
    echo "$RUST_TARGET" > /rust_target

COPY . .

# Cross linker per target (each var is consulted only when building that target,
# and resolves to the native gcc when building that arch natively).
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc

# The workspace release profile uses fat LTO + codegen-units=1, whose final
# link of crw-server (aws-lc-sys + the full dep graph) needs several GB and
# OOM-killed the docker build (see #90's 4 GB OOM warning). The container
# binary doesn't need max LTO, so use thin LTO across more codegen units —
# far lower peak memory and a faster link, negligible runtime difference.
ENV CARGO_PROFILE_RELEASE_LTO=thin \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16

RUN set -eux; \
    RUST_TARGET="$(cat /rust_target)"; \
    cargo build --release --target "$RUST_TARGET" \
      -p crw-server --features cdp,camofox -p crw-mcp -p crw-cli -p crw-browse-camofox; \
    mkdir -p /out; \
    cp "target/${RUST_TARGET}/release/crw" \
       "target/${RUST_TARGET}/release/crw-server" \
       "target/${RUST_TARGET}/release/crw-mcp" \
       "target/${RUST_TARGET}/release/crw-browse-camofox" /out/

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /out/crw /usr/local/bin/crw
COPY --from=builder /out/crw-server /usr/local/bin/crw-server
COPY --from=builder /out/crw-mcp /usr/local/bin/crw-mcp
COPY --from=builder /out/crw-browse-camofox /usr/local/bin/crw-browse-camofox
COPY config.default.toml /app/config.default.toml
COPY config.docker.toml /app/config.docker.toml

WORKDIR /app

LABEL org.opencontainers.image.source=https://github.com/adambenhassen/crw-camofox
LABEL io.modelcontextprotocol.server.name="io.github.us/crw"

EXPOSE 3000

CMD ["crw-server"]
