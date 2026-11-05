# Build environment for the webOS client (host gcc is for build scripts): the community webOS toolchain (openlgtv/buildroot-nc4,
# the one moonlight-tv uses) plus Rust's armv7 soft-float-ABI target, which matches it.
FROM debian:bookworm-slim

ARG TARGETARCH
ARG TOOLCHAIN=2026.08-webos
ARG RUST=1.88.0

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl file gcc libc6-dev python3 xz-utils \
    && rm -rf /var/lib/apt/lists/*

RUN host=$([ "$TARGETARCH" = arm64 ] && echo aarch64 || echo x86_64) \
    && curl -fsSL "https://github.com/openlgtv/buildroot-nc4/releases/download/$TOOLCHAIN/arm-webos-linux-gnueabi_sdk-buildroot-$host.tar.gz" \
       | tar -xz -C /opt \
    && /opt/arm-webos-linux-gnueabi_sdk-buildroot/relocate-sdk.sh

ENV PATH=/opt/arm-webos-linux-gnueabi_sdk-buildroot/bin:/root/.cargo/bin:$PATH \
    CARGO_HOME=/root/.cargo \
    CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABI_LINKER=arm-webos-linux-gnueabi-gcc \
    CC_armv7_unknown_linux_gnueabi=arm-webos-linux-gnueabi-gcc \
    AR_armv7_unknown_linux_gnueabi=arm-webos-linux-gnueabi-ar \
    CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABI_RUSTFLAGS='--cfg getrandom_backend="linux_raw"'

# Why linux_raw: webOS 4's glibc (2.24) has no getrandom(), and the developer-mode jail has /dev/urandom but
# not /dev/random, which getrandom's file fallback polls first. The kernel (4.4) has the syscall.

RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain $RUST \
    && rustup target add armv7-unknown-linux-gnueabi

WORKDIR /src
