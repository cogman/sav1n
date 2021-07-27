FROM debian:bullseye-slim AS runtime

ENV LD_LIBRARY_PATH=$LD_LIBRARY_PATH:/usr/local/lib:/usr/local/lib/x86_64-linux-gnu/:/usr/local/lib/vapoursynth

RUN apt-get update && apt-get install -y \
    python3 \
    libass9 \
    libfreetype6 \
    libgnutls30  \
    libgnutlsxx28 \
    libgnutls-openssl27 \
    libgnutls-dane0 \
    libpython3.9 \
    libsdl2-2.0-0 \
    libva2  \
    libvdpau1 \
    libxcb1 \
    libxcb-shm0 \
    libxcb-xfixes0 \
    texinfo \
    zlib1g \
    && rm -rf /var/lib/apt/lists/*

FROM rust:slim-bullseye AS rustBuild
WORKDIR /sav1n

COPY src/ src/
COPY Cargo.toml .
COPY Cargo.lock .
ENV RUSTFLAGS "-Zsanitizer=address"
ENV RUSTDOCFLAGS "-Zsanitizer=address"
RUN rustup install nightly
RUN rustup toolchain install nightly --component rust-src
RUN cargo +nightly test -Zbuild-std --target x86_64-unknown-linux-gnu

ENV RUSTFLAGS "-C target-cpu=znver1"
RUN cargo build --release

FROM runtime AS build
ARG CFLAGS="-O3 -march=znver1 -flto -fPIC -s"
ARG CXXFLAGS="-O3 -march=znver1 -flto -fPIC -s"
RUN apt-get update && apt-get install -y \
    autoconf \
    automake \
    build-essential \
    cmake \
    git \
    git-core \
    libass-dev \
    libfreetype6-dev \
    libgnutls28-dev \
    libsdl2-dev \
    libtool \
    libva-dev \
    libvdpau-dev \
    libxcb1-dev \
    libxcb-shm0-dev \
    libxcb-xfixes0-dev \
    nasm \
    ninja-build \
    perl \
    pkg-config \
    python3-pip \
    texinfo \
    wget \
    yasm \
    zlib1g-dev \
    && rm -rf /var/lib/apt/lists/*

RUN pip3 --no-cache-dir install meson setuptools cython sphinx

FROM build AS vapoursynth
ARG CFLAGS="-O3 -march=znver1 -flto -fPIC -s"
ARG CXXFLAGS="-O3 -march=znver1 -flto -fPIC -s"
RUN mkdir -p /vapoursynth/dependencies && git clone https://github.com/sekrit-twc/zimg -b master --depth=1 /vapoursynth/dependencies/zimg
WORKDIR /vapoursynth/dependencies/zimg
RUN ./autogen.sh  && \
    ./configure --enable-x86simd --disable-static --enable-shared && \
    make && \
    make install

RUN git clone https://github.com/vapoursynth/vapoursynth.git --depth=1 -b master /vapoursynth/build
WORKDIR /vapoursynth/build
RUN ./autogen.sh && \
    ./configure --enable-shared && \
    make && \
    make install

FROM build AS aom
ARG CFLAGS="-O3 -march=znver1 -flto -fPIC -s"
ARG CXXFLAGS="-O3 -march=znver1 -flto -fPIC -s"
RUN git clone https://aomedia.googlesource.com/aom --depth=1 -b master /aom
WORKDIR /aom_build
RUN cmake -DBUILD_SHARED_LIBS=1 -DCMAKE_BUILD_TYPE=Release /aom && \
    make && \
    make install

FROM build AS dav1d
WORKDIR /dav1d
RUN git -C dav1d pull 2> /dev/null || git clone --depth 1 https://code.videolan.org/videolan/dav1d.git && \
    mkdir -p dav1d/build && \
    cd dav1d/build && \
    meson setup -Denable_tools=false --default-library=static ..  && \
    ninja && \
    ninja install

FROM build AS opus
WORKDIR /opus
RUN git -C opus pull 2> /dev/null || git clone --depth 1 https://github.com/xiph/opus.git && \
    cd opus && \
    ./autogen.sh && \
    ./configure && \
    make && \
    make install

FROM build AS vmaf
WORKDIR /vmaf
RUN wget https://github.com/Netflix/vmaf/archive/v2.1.1.tar.gz && \
    tar xvf v2.1.1.tar.gz && \
    mkdir -p vmaf-2.1.1/libvmaf/build &&\
    cd vmaf-2.1.1/libvmaf/build && \
    meson setup -Denable_tests=false -Denable_docs=false --buildtype=release .. && \
    ninja && \
    ninja install

FROM build AS vpx
RUN git -C libvpx pull 2> /dev/null || git clone --depth 1 https://chromium.googlesource.com/webm/libvpx.git && \
    cd libvpx && \
    ./configure --disable-unit-tests --enable-vp9-highbitdepth --enable-shared --enable-tools --as=yasm --enable-vp9 && \
    make && \
    make install


FROM build AS ffmpeg

COPY --from=aom /usr/local/include /usr/local/include
COPY --from=aom /usr/local/lib /usr/local/lib

COPY --from=dav1d /usr/local/include /usr/local/include
COPY --from=dav1d /usr/local/lib /usr/local/lib

COPY --from=opus /usr/local/include /usr/local/include
COPY --from=opus /usr/local/lib /usr/local/lib

COPY --from=vmaf /usr/local/include /usr/local/include
COPY --from=vmaf /usr/local/lib /usr/local/lib

COPY --from=vpx /usr/local/include /usr/local/include
COPY --from=vpx /usr/local/lib /usr/local/lib

WORKDIR /ffmpeg
RUN wget -O ffmpeg-snapshot.tar.bz2 https://ffmpeg.org/releases/ffmpeg-snapshot.tar.bz2 && \
    tar xjvf ffmpeg-snapshot.tar.bz2 && \
    cd ffmpeg && \
    ./configure \
      --disable-doc \
      --extra-libs="-lpthread -lm" \
      --ld="g++" \
      --enable-gpl \
      --enable-gnutls \
      --enable-libaom \
      --enable-libass \
      --enable-libfreetype \
      --enable-libopus \
      --enable-libdav1d \
      --enable-libvmaf \
      --enable-libvpx \
      --enable-nonfree && \
    make && \
    make install

FROM runtime
WORKDIR /sav1n

COPY --from=vapoursynth /usr/local/lib/*.so* /usr/local/lib/
COPY --from=vapoursynth /usr/local/lib/*.la* /usr/local/lib/
COPY --from=vapoursynth /usr/local/lib/vapoursynth /usr/local/lib/vapoursynth
COPY --from=vapoursynth /usr/local/lib/python3.9/site-packages /usr/local/lib/python3.9/site-packages

COPY --from=vapoursynth /usr/local/bin/vspipe /usr/local/bin
COPY --from=vpx /usr/local/bin/vpxenc /usr/local/bin
COPY --from=vmaf /usr/local/share /usr/local/share
COPY --from=aom /usr/local/bin/aomenc /usr/local/bin
COPY --from=ffmpeg /usr/local/bin/ffmpeg /usr/local/bin
COPY --from=ffmpeg /usr/local/lib/*.so* /usr/local/lib/

COPY --from=rustBuild /sav1n/target/release/sav1n .

ENV PATH="/sav1n:/usr/local/bin:${PATH}"
ENV PYTHONPATH=/usr/local/lib/python3.9/site-packages
WORKDIR /video
ENTRYPOINT ["sav1n"]
