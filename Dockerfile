FROM debian:bullseye-slim AS runtime

ARG CFLAGS="-fno-omit-frame-pointer -pthread -fgraphite-identity -floop-block -ldl -lpthread -g -fPIC"
ARG CXXFLAGS="-fno-omit-frame-pointer -pthread -fgraphite-identity -floop-block -ldl -lpthread -g -fPIC"
ARG LDFLAGS="-Wl,-Bsymbolic -fPIC"
ENV LD_LIBRARY_PATH=$LD_LIBRARY_PATH:/usr/local/lib:/usr/local/lib/x86_64-linux-gnu/:/usr/local/lib/vapoursynth

RUN apt-get update && apt-get install -y \
    python3 \
    libpython3.9 \
    && rm -rf /var/lib/apt/lists/*

FROM rust:slim-bullseye AS rustBuild

WORKDIR /sav1n
COPY . .
ENV RUSTFLAGS "-Zsanitizer=address"
ENV RUSTDOCFLAGS "-Zsanitizer=address"
RUN rustup install nightly
RUN rustup toolchain install nightly --component rust-src
RUN cargo +nightly test -Zbuild-std --target x86_64-unknown-linux-gnu
ENV RUSTFLAGS "-C target-cpu=znver1"
RUN cargo build --release

FROM runtime AS build
RUN apt-get update && apt-get install -y \
    autoconf \
    automake \
    build-essential \
    git \
    libtool \
    pkg-config \
    python3-pip \
    && rm -rf /var/lib/apt/lists/*

RUN pip3 --no-cache-dir install meson setuptools cython sphinx

RUN mkdir -p /vapoursynth/dependencies && git clone https://github.com/sekrit-twc/zimg -b master --depth=1 /vapoursynth/dependencies/zimg
WORKDIR /vapoursynth/dependencies/zimg
RUN ./autogen.sh  && \
    ./configure --enable-x86simd --disable-static --enable-shared && \
    make -j"$(nproc)" && \
    make install

RUN git clone https://github.com/vapoursynth/vapoursynth.git /vapoursynth/build
WORKDIR /vapoursynth/build
RUN ./autogen.sh && \
    ./configure --enable-shared && \
    make -j"$(nproc)" && \
    make install

FROM runtime
WORKDIR /sav1n

COPY --from=build /usr/local/lib /usr/local/lib
COPY --from=build /usr/local/bin /usr/local/bin
COPY --from=rustBuild /sav1n/target/release/sav1n .

ENV PATH="/sav1n:/usr/local/bin:${PATH}"
ENV PYTHONPATH=/usr/local/lib/python3.9/site-packages
WORKDIR /video
ENTRYPOINT ["sav1n"]
