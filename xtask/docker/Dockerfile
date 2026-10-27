# mothershipOS build environment: cross-compiles the Cargo workspace for the phone (aarch64, Ubuntu 26.04) and the
# Linux guests' kernel.
# The arm64 -dev packages come from the phone's own archive (multiarch), so binaries link against exactly the
# libraries the phone runs (GTK 4, libadwaita, libdrm, wlroots, ...). Built and used by `cargo xtask`.
# Tasks that build in an arm64 image of their own (guest-mesa) use the host's Docker: run it with
# /var/run/docker.sock mounted and MSHIP_HOST_TARGET set to the host path of the target directory.
FROM ubuntu:26.04
ENV DEBIAN_FRONTEND=noninteractive
RUN dpkg --add-architecture arm64 \
 && sed -i 's/^Types: deb$/Types: deb\nArchitectures: amd64 arm64/' /etc/apt/sources.list.d/ubuntu.sources \
 && apt-get update -qq \
 && apt-get install -y -qq --no-install-recommends \
	ca-certificates curl git pkg-config build-essential clang \
	gcc-aarch64-linux-gnu g++-aarch64-linux-gnu libc6-dev-arm64-cross \
	libgtk-4-dev:arm64 libadwaita-1-dev:arm64 libdrm-dev:arm64 libwlroots-0.19-dev:arm64 \
	libwayland-dev:arm64 wayland-protocols libinput-dev:arm64 libudev-dev:arm64 libgbm-dev:arm64 \
	libdbus-1-dev:arm64 \
	flex bison bc gawk libssl-dev libelf-dev cpio kmod meson ninja-build libva-dev:arm64 \
	docker.io docker-buildx dpkg-dev device-tree-compiler u-boot-tools f2fs-tools unzip \
	libepoxy-dev:arm64 libcap-dev:arm64 libclang-dev protobuf-compiler python3-yaml libwayland-bin \
	libasound2-dev:arm64 libjson-c-dev:arm64 libpcre2-dev:arm64 libpango1.0-dev:arm64 libcairo2-dev:arm64 libevdev-dev:arm64 \
	libgdk-pixbuf-2.0-dev:arm64 glslang-tools hwdata \
 && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain stable \
	--target aarch64-unknown-linux-gnu --component clippy,rustfmt \
 && chmod -R a+rwX /opt/cargo /opt/rustup
# cross pkg-config: the arm64 .pc files; linker: the aarch64 gcc
ENV PKG_CONFIG_ALLOW_CROSS=1 \
    PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig:/usr/share/pkgconfig \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ \
    BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_linux_gnu="--sysroot=/ -I/usr/include/aarch64-linux-gnu"
WORKDIR /src
