ARG UBUNTU=ubuntu:26.04@sha256:d66b366dac61b575bee2aef07e9c26e8070096fbe84a3d4d3eb0da069b5d76db

FROM ${UBUNTU} AS systemd
ARG BUILD_JOBS=4
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl gcc gcc-15 g++ binutils libcap-dev meson ninja-build \
    pkgconf gperf python3-jinja2 python3-packaging xz-utils \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
ADD --checksum=sha256:116432ee9dce9bfef4c7da7963c6de41899ab44197f2876ee99545707a5c7c1f https://codeload.github.com/systemd/systemd/tar.gz/refs/tags/v262.2 /tmp/systemd.tar.gz
RUN tar -xf /tmp/systemd.tar.gz --strip-components=1 \
    && meson setup static --default-library=static --prefer-static --auto-features=disabled \
       -Dmode=release -Dbuild-static=true -Dsystemd-multicall-binary=true -Dtranslations=false \
       -Dbuildtype=minsize -Db_lto=true \
    && ninja -C static -j "$BUILD_JOBS" systemd \
    && meson setup build-tools --auto-features=disabled -Dmode=release -Dtranslations=false \
       -Dstandalone-binaries=systemctl,systemd-run \
    && ninja -C build-tools -j "$BUILD_JOBS" systemctl.standalone systemd-run.standalone \
    && mkdir /out && install -s static/systemd /out/systemd \
    && install -s build-tools/systemctl.standalone /out/systemctl \
    && install -s build-tools/systemd-run.standalone /out/systemd-run \
    && install -m 0644 src/core/org.freedesktop.systemd1.conf /out/ \
    && ! readelf -l /out/systemd | grep -q INTERP \
    && ! readelf -d /out/systemd | grep -q NEEDED

FROM rust:1.99.1-bookworm AS harness
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml LICENSE ./
COPY server server
COPY harness harness
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target,sharing=locked \
    cargo build -p venticode-harness --release --locked -j 2 \
    && mkdir /out && cp target/release/venticode-harness target/release/venticode-cli /out/

FROM ${UBUNTU}
ARG TARGETARCH
ENV container=oci
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl dbus dbus-x11 ffmpeg fonts-dejavu-core git libasound2t64 \
    libgbm1 libgtk-3-0t64 libnss3 libxss1 libxkbcommon0 procps python3 \
    thunar xauth xclip xdotool xfce4-panel xfce4-session xfce4-terminal xfdesktop4 xfwm4 xvfb xz-utils \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 venticode && useradd --uid 10001 --gid 10001 --no-create-home --home-dir /data/home venticode \
    && groupadd --gid 10002 desktop && useradd --uid 10002 --gid 10002 --no-create-home --home-dir /data/desktop --groups venticode desktop
RUN case "$TARGETARCH" in \
      amd64) arch=x86_64; digest=e415898b51bf1c41d6bd7d44027776c99bf36d9e0ee7416c1d8bf0de53b6ea0c ;; \
      arm64) arch=arm64; digest=1c3df8417891cdbe31d7bf7bc3d71625490c4d15a52ae4b556a310aea4f06e9f ;; \
      *) exit 1 ;; \
    esac \
    && curl -fsSL --retry 3 "https://github.com/imputnet/helium-linux/releases/download/0.22.1.1/helium-0.22.1.1-${arch}_linux.tar.xz" -o /tmp/helium.tar.xz \
    && printf '%s  /tmp/helium.tar.xz\n' "$digest" | sha256sum -c - \
    && mkdir -p /opt/helium && tar -xJf /tmp/helium.tar.xz --strip-components=1 -C /opt/helium \
    && test -x /opt/helium/helium && find /opt/helium -type f -perm /6000 -exec chmod a-s {} + \
    && rm /tmp/helium.tar.xz
COPY --from=systemd /out/systemd /usr/lib/systemd/systemd
COPY --from=systemd /out/systemctl /out/systemd-run /usr/local/bin/
COPY --from=systemd /out/org.freedesktop.systemd1.conf /usr/share/dbus-1/system.d/
COPY --from=harness /out/ /usr/local/bin/
COPY computer/units/ /etc/systemd/system/
COPY computer/desktop.py computer/init.py /usr/local/lib/venticode/
RUN ln -sf /etc/systemd/system/venticode.target /etc/systemd/system/default.target
STOPSIGNAL SIGRTMIN+3
ENTRYPOINT ["/usr/bin/python3", "/usr/local/lib/venticode/init.py"]
