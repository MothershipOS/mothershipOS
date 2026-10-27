# Toolchain the nio host kernel is built with (Ubuntu 24.04: aarch64-linux-gnu-gcc 13.3.0, binutils 2.42). Using the
# same compiler as the running kernel keeps incremental builds and existing modules valid.
# Used by `cargo xtask host-kernel` (image mothership-host-kbuild:24.04).
FROM ubuntu:24.04
RUN apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
	gcc gcc-aarch64-linux-gnu binutils-aarch64-linux-gnu libc6-dev make bc flex bison libssl-dev libelf-dev python3 cpio kmod \
	ca-certificates >/dev/null && rm -rf /var/lib/apt/lists/*
