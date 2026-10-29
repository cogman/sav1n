# syntax=docker/dockerfile:1

FROM debian:trixie-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad AS runtime

ENV LD_LIBRARY_PATH=$LD_LIBRARY_PATH:/usr/local/lib:/usr/local/lib/x86_64-linux-gnu/:/usr/local/lib/vapoursynth:/usr/local/lib/python3.13/dist-packages/vapoursynth

RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update && apt-get install -y \
    alien \
    clinfo \
    libass9 \
    libfftw3-bin \
    libpython3.13 \
    libvdpau1 \
    libva2 \
    libva-drm2 \
    libxcb1 \
    mkvtoolnix \
    ocl-icd-libopencl1 \
    python3 \
    xxhash

FROM rust:slim-trixie@sha256:1a939748a7cbab172de91a014e3804a97af1394a8c844faa67fce9401c4e40fb AS rustbuild
WORKDIR /sav1n
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update && apt-get install -y make fd-find
COPY src/ src/
COPY Cargo.toml .
COPY Cargo.lock .
ENV RUSTFLAGS "-Zsanitizer=address"
ENV RUSTDOCFLAGS "-Zsanitizer=address"
RUN rustup install nightly
RUN rustup toolchain install nightly --component rust-src
RUN --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/usr/local/cargo/registry/ \
    cargo +nightly test -Zbuild-std --target x86_64-unknown-linux-gnu

ENV RUSTFLAGS "-C target-cpu=x86-64-v3"
RUN --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/usr/local/cargo/registry/ \
    cargo build --release

FROM runtime AS build
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update && apt-get install -y \
    autoconf \
    automake \
    build-essential \
    cmake \
    git \
    libass-dev \
    libboost-dev \
    libboost-filesystem-dev \
    libfftw3-dev \
    libfreetype-dev \
    libtool \
    libva-dev \
    libvdpau-dev \
    nasm \
    ninja-build \
    ocl-icd-opencl-dev \
    perl \
    pkg-config \
    python3-dev \
    python3-pip \
    texinfo \
    wget \
    libxxhash-dev \
    yasm \
    zlib1g-dev

RUN --mount=type=cache,target=/root/.cache/pip \
    pip3 --no-cache-dir install --break-system-packages "cython>=3.3" meson setuptools sphinx

ENV CFLAGS "-O3 -march=x86-64-v3 -fPIC"
ENV CXXFLAGS "-O3 -march=x86-64-v3 -fPIC"

FROM build AS vapoursynth
RUN mkdir -p /vapoursynth/dependencies && git clone https://github.com/sekrit-twc/zimg -b master --depth=1 /vapoursynth/dependencies/zimg
WORKDIR /vapoursynth/dependencies/zimg
RUN git submodule update --init --recursive
RUN ./autogen.sh  && \
    ./configure --enable-x86simd --disable-static --enable-shared --with-plugindir=/usr/local/lib/vapoursynth || cat config.log && \
    make && \
    make install

RUN git clone https://github.com/vapoursynth/vapoursynth.git --depth=1 -b R81 /vapoursynth/build
WORKDIR /vapoursynth/build
RUN meson setup /vapoursynth/vs-build --prefix=/usr/local --buildtype=release \
    -Dpython.purelibdir=/usr/local/lib/python3.13/dist-packages \
    -Dpython.platlibdir=/usr/local/lib/python3.13/dist-packages && \
    meson compile -C /vapoursynth/vs-build && \
    meson install -C /vapoursynth/vs-build

RUN mkdir -p /usr/local/include/vapoursynth && \
    cp /usr/local/lib/python3.13/dist-packages/vapoursynth/include/*.h /usr/local/include/vapoursynth/ && \
    printf '%s\n' \
    'prefix=/usr/local' \
    'exec_prefix=${prefix}' \
    'libdir=/usr/local/lib' \
    'includedir=/usr/local/lib/python3.13/dist-packages/vapoursynth/include' \
    '' \
    'Name: vapoursynth' \
    'Description: A frameserver for the 21st century' \
    'Version: 81' \
    'Cflags: -I${includedir}' \
    > /usr/local/lib/pkgconfig/vapoursynth.pc && \
    printf '%s\n' \
    '#!/bin/sh' \
    'exec /usr/local/lib/python3.13/dist-packages/vapoursynth/vspipe "$@"' \
    > /usr/local/bin/vspipe && \
    chmod +x /usr/local/bin/vspipe

RUN mkdir -p /usr/local/lib/vapoursynth /usr/local/lib/python3/dist-packages/vapoursynth && \
    ln -sfn /usr/local/lib/vapoursynth /usr/local/lib/python3/dist-packages/vapoursynth/plugins && \
    printf '%s\n' \
    '#!/bin/sh' \
    'exec python3 -m vapoursynth "$@"' \
    > /usr/local/bin/vapoursynth && \
    chmod +x /usr/local/bin/vapoursynth

FROM vapoursynth AS aliceports
RUN git clone https://github.com/AliceTeaParty/vapoursynth-api4-wheels.git /alice && \
    git -C /alice checkout 0b16c23ddfa15a3dcb72f17007f1ffd9b44b100e

FROM vapoursynth AS miscfilters
RUN git clone https://github.com/vapoursynth/vs-miscfilters-obsolete.git --depth=1 -b master /vs-misc
WORKDIR /vs-misc
RUN meson build && \
    ninja -C build

FROM vapoursynth AS removegrain
RUN git clone https://github.com/vapoursynth/vs-removegrain.git --depth=1 -b master /removegrain
WORKDIR /removegrain
RUN meson build && \
    ninja -C build

FROM vapoursynth AS vivtc
RUN git clone https://github.com/vapoursynth/vivtc.git --depth=1 -b master /vivtc
WORKDIR /vivtc
RUN meson build && \
    ninja -C build

FROM vapoursynth AS addgrain
RUN git clone https://github.com/HomeOfVapourSynthEvolution/VapourSynth-AddGrain.git --depth=1 -b master /addgrain
WORKDIR /addgrain
RUN meson build && \
    ninja -C build && \
    ninja -C build install

FROM vapoursynth AS vcas
RUN git clone https://github.com/HomeOfVapourSynthEvolution/VapourSynth-CAS.git --depth=1 -b master /cas
WORKDIR /cas
RUN meson build && \
    ninja -C build && \
    ninja -C build install

FROM vapoursynth AS dct
RUN git clone https://github.com/HomeOfVapourSynthEvolution/VapourSynth-DCTFilter.git --depth=1 -b master /dct
WORKDIR /dct
RUN meson build && \
    ninja -C build && \
    ninja -C build install

FROM vapoursynth AS deblock
RUN git clone https://github.com/HomeOfVapourSynthEvolution/VapourSynth-Deblock.git --depth=1 -b master /deblock
WORKDIR /deblock
RUN meson build && \
    ninja -C build && \
    ninja -C build install

FROM aliceports AS dfttest
WORKDIR /alice/plugins/vapoursynth-dfttest
RUN meson build && \
    ninja -C build && \
    ninja -C build install

FROM vapoursynth AS eedi3
RUN git clone https://github.com/HomeOfVapourSynthEvolution/VapourSynth-EEDI3.git --depth=1 -b master /eedi3
WORKDIR /eedi3
RUN sed -i '1i #include <cfloat>' EEDI3/EEDI3.cpp EEDI3/EEDI3_SSE4.cpp EEDI3/EEDI3_AVX2.cpp && \
    meson build && \
    ninja -C build && \
    ninja -C build install

FROM vapoursynth AS fft3dfilter
RUN git clone https://github.com/myrsloik/VapourSynth-FFT3DFilter.git --depth=1 -b master /fft3dfilter
WORKDIR /fft3dfilter
RUN meson build && \
    ninja -C build && \
    ninja -C build install

FROM vapoursynth AS fmtconv
RUN git clone https://github.com/AmusementClub/fmtconv.git --depth=1 -b master /fmtconv
WORKDIR /fmtconv/build/unix
RUN ./autogen.sh && \
    ./configure && \
    make

FROM vapoursynth AS knlmeanscl
RUN git clone https://github.com/Jaded-Encoding-Thaumaturgy/vapoursynth-knlmeanscl.git --depth=1 -b master /knlmeanscl
WORKDIR /knlmeanscl
RUN meson build && \
    ninja -C build

FROM vapoursynth AS mvtools
RUN git clone https://github.com/dubhater/vapoursynth-mvtools.git --depth=1 -b master /mvtools
WORKDIR /mvtools
RUN sed -i 's/#include <cmath>/#include <cmath>\n#include <math.h>/' src/MVDepan.cpp && \
    meson build && \
    ninja -C build

FROM aliceports AS nnedi3cl
WORKDIR /alice/plugins/vapoursynth-nnedi3cl
RUN meson build && \
    ninja -C build && \
    ninja -C build install && \
    mkdir -p /usr/local/share/NNEDI3CL && \
    cp NNEDI3CL/nnedi3_weights.bin /usr/local/share/NNEDI3CL/

FROM vapoursynth AS sangnom
RUN git clone https://github.com/dubhater/vapoursynth-sangnom.git --depth=1 -b master /sangnom
WORKDIR /sangnom
RUN meson build && \
    ninja -C build

FROM vapoursynth AS ttempsmooth
RUN git clone https://github.com/HomeOfVapourSynthEvolution/VapourSynth-TTempSmooth.git --depth=1 -b master /ttempsmooth
WORKDIR /ttempsmooth
RUN meson build && \
    ninja -C build

FROM vapoursynth AS znedi3
RUN git clone https://github.com/sekrit-twc/znedi3.git -n /znedi3
WORKDIR /znedi3
RUN git checkout 34b2aa9f5d10c31775469b7a358f1ff52375ed1e && \
    git submodule init && \
    git submodule update --recursive
RUN make x86=1

FROM vapoursynth AS havsfunc
RUN git clone https://github.com/dubhater/vapoursynth-adjust.git --depth=1 -b master /adjust
RUN mv /adjust/adjust.py /usr/local/lib/python3.13/dist-packages

RUN git clone https://github.com/AmusementClub/mvsfunc.git --depth=1 -b mod /mvsfunc
RUN mv /mvsfunc/mvsfunc.py /usr/local/lib/python3.13/dist-packages

RUN git clone https://github.com/mawen1250/VapourSynth-script.git --depth=1 -b master /nnedi3_resample
RUN mv /nnedi3_resample/nnedi3_resample.py /usr/local/lib/python3.13/dist-packages

RUN git clone https://github.com/Irrational-Encoding-Wizardry/vsutil.git --depth=1 -b master /vsutil
RUN mv /vsutil/vsutil /usr/local/lib/python3.13/dist-packages

RUN --mount=type=cache,target=/root/.cache/pip \
    pip3 --no-cache-dir install --break-system-packages --no-deps --target /vspython \
    havsfunc \
    vsjetpack \
    jetpytools \
    numpy \
    psutil \
    rich \
    markdown-it-py \
    mdurl \
    typing_extensions \
    vapoursynth-akarin \
    vapoursynth-vszip

COPY --from=addgrain /usr/local/lib/vapoursynth /usr/local/lib/vapoursynth
COPY --from=vcas /usr/local/lib/vapoursynth /usr/local/lib/vapoursynth
COPY --from=dct /usr/local/lib/vapoursynth /usr/local/lib/vapoursynth
COPY --from=deblock /usr/local/lib/vapoursynth /usr/local/lib/vapoursynth
COPY --from=dfttest /alice/plugins/vapoursynth-dfttest/build/*.so /usr/local/lib/vapoursynth
COPY --from=eedi3 /usr/local/lib/vapoursynth /usr/local/lib/vapoursynth
COPY --from=fft3dfilter /usr/local/lib/vapoursynth /usr/local/lib/vapoursynth
COPY --from=fmtconv /fmtconv/build/unix/.libs/*.so /usr/local/lib/vapoursynth
COPY --from=knlmeanscl /knlmeanscl/build/*.so /usr/local/lib/vapoursynth
COPY --from=mvtools /mvtools/build/*.so /usr/local/lib/vapoursynth
COPY --from=nnedi3cl /alice/plugins/vapoursynth-nnedi3cl/build/*.so /usr/local/lib/vapoursynth
COPY --from=sangnom /sangnom/build/*.so /usr/local/lib/vapoursynth
COPY --from=ttempsmooth /ttempsmooth/build/*.so /usr/local/lib/vapoursynth
COPY --from=znedi3 /znedi3/*.so /usr/local/lib/vapoursynth
COPY --from=znedi3 /znedi3/nnedi3_weights.bin /usr/local/lib/vapoursynth

FROM build AS aom
RUN git clone https://aomedia.googlesource.com/aom --depth=1 -b main /aom
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
RUN wget https://github.com/Netflix/vmaf/archive/v3.2.1.tar.gz && \
    tar xvf v3.2.1.tar.gz
RUN mkdir -p vmaf-3.2.1/libvmaf/build &&\
    cd vmaf-3.2.1/libvmaf/build && \
    meson setup -Denable_tests=false -Denable_docs=false --buildtype=release .. && \
    ninja -v 2>&1 && \
    ninja install

FROM build AS vpx
RUN git -C libvpx pull 2> /dev/null || git clone --depth 1 https://chromium.googlesource.com/webm/libvpx.git && \
    cd libvpx && \
    ./configure --disable-unit-tests --enable-vp9-highbitdepth --enable-shared --enable-tools --as=yasm --enable-vp9 && \
    make && \
    make install

FROM build AS x265
RUN git clone --depth=1 -b 4.2 https://bitbucket.org/multicoreware/x265_git.git /x265
WORKDIR /x265/build/linux
RUN cmake -G "Unix Makefiles" \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX=/usr/local \
    -DENABLE_SHARED=ON \
    -DENABLE_CLI=ON \
    -DHIGH_BIT_DEPTH=ON \
    /x265/source && \
    make && \
    make install


FROM build AS ffmpeg

COPY --from=aom /usr/local/include /usr/local/include
COPY --from=aom /usr/local/lib /usr/local/lib

COPY --from=dav1d /usr/local/include /usr/local/include
COPY --from=dav1d /usr/local/lib /usr/local/lib

COPY --from=opus /usr/local/include /usr/local/include
COPY --from=opus /usr/local/lib /usr/local/lib

COPY --from=vapoursynth /usr/local/include /usr/local/include
COPY --from=vapoursynth /usr/local/lib /usr/local/lib

COPY --from=vmaf /usr/local/include /usr/local/include
COPY --from=vmaf /usr/local/lib /usr/local/lib

COPY --from=vpx /usr/local/include /usr/local/include
COPY --from=vpx /usr/local/lib /usr/local/lib

COPY --from=x265 /usr/local/include /usr/local/include
COPY --from=x265 /usr/local/lib /usr/local/lib

WORKDIR /ffmpeg
RUN git clone --branch 'release/8.1' https://github.com/FFmpeg/FFmpeg.git --depth 1 ffmpeg  && \
    cd ffmpeg && \
    ./configure \
    --disable-doc \
    --disable-static \
    --enable-shared \
    --enable-pic \
    --extra-libs="-lpthread -lm" \
    --ld="g++" \
    --enable-gpl \
    --enable-libaom \
    --enable-libass \
    --enable-libfreetype \
    --enable-libopus \
    --enable-libdav1d \
    --enable-libvmaf \
    --enable-libvpx \
    --enable-libx265 \
    --enable-vapoursynth \
    --enable-nonfree && \
    make -j`nproc` && \
    make install

FROM ffmpeg AS ffms2
RUN git clone https://github.com/FFMS/ffms2.git --depth 1 /ffms2 && mkdir -p /ffms2/src/config
WORKDIR /ffms2/
RUN autoreconf -fiv && \
    ./configure --enable-shared  && \
    make

FROM ffmpeg AS lsmash
RUN git clone https://github.com/dwbuiten/obuparse --depth 1 /obuparse
WORKDIR /obuparse
RUN make && \
    make install

RUN git clone https://github.com/vimeo/l-smash --depth 1 /lsmash
WORKDIR /lsmash
RUN ./configure --enable-shared && \
    make && \
    make install

RUN git clone https://github.com/HomeOfAviSynthPlusEvolution/L-SMASH-Works.git --depth 1 /lsmash-plugin && mkdir -p /lsmash-plugin/build-vapoursynth /lsmash-plugin/build-avisynth
WORKDIR /lsmash-plugin/build-vapoursynth
RUN meson "../VapourSynth" && \
    ninja && \
    ninja install

FROM runtime
ARG UID=1000
ARG GID=1000
WORKDIR /sav1n

COPY --from=vapoursynth /usr/local/lib/*.so* /usr/local/lib/
COPY --from=vapoursynth /usr/local/lib/python3.13/dist-packages /usr/local/lib/python3.13/dist-packages

COPY --from=vapoursynth /usr/local/bin/vspipe /usr/local/bin/
COPY --from=vpx /usr/local/bin/vpxenc /usr/local/bin/
COPY --from=x265 /usr/local/bin/x265 /usr/local/bin/
COPY --from=x265 /usr/local/lib/libx265.so* /usr/local/lib/
COPY --from=vmaf /vmaf/vmaf-3.2.1/model/vmaf_v0.6.1.json /usr/local/share/model/
COPY --from=aom /usr/local/bin/aomenc /usr/local/bin/
COPY --from=ffmpeg /usr/local/bin/ffmpeg /usr/local/bin/
COPY --from=ffmpeg /usr/local/bin/ffprobe /usr/local/bin/
COPY --from=ffmpeg /usr/local/lib/*.so* /usr/local/lib/
COPY --from=ffmpeg /usr/local/lib/x86_64-linux-gnu/*.so* /usr/local/lib/x86_64-linux-gnu/
COPY --from=ffms2 /ffms2/src/core/.libs/libffms2.so /usr/local/lib/vapoursynth/
COPY --from=lsmash /usr/local/lib/*.so* /usr/local/lib/
COPY --from=lsmash /usr/local/lib/vapoursynth /usr/local/lib/vapoursynth/
COPY --from=havsfunc /usr/local/lib/python3.13/dist-packages/*.py /usr/local/lib/python3.13/dist-packages/
COPY --from=havsfunc /usr/local/lib/python3.13/dist-packages/vsutil/ /usr/local/lib/python3.13/dist-packages/vsutil/
COPY --from=havsfunc /vspython/ /usr/local/lib/python3.13/dist-packages/
COPY --from=havsfunc /usr/local/lib/vapoursynth /usr/local/lib/vapoursynth/
COPY --from=znedi3 /znedi3/nnedi3_weights.bin /usr/local/share/nnedi3/
COPY --from=nnedi3cl /usr/local/share/NNEDI3CL/nnedi3_weights.bin /usr/local/share/NNEDI3CL/
COPY --from=miscfilters /vs-misc/build/libmiscfilters.so /usr/local/lib/vapoursynth/
COPY --from=removegrain /removegrain/build/libremovegrain.so /usr/local/lib/vapoursynth/
COPY --from=vivtc /vivtc/build/*.so /usr/local/lib/vapoursynth/

COPY --from=rustbuild /sav1n/target/release/sav1n .

ENV PATH="/sav1n:/usr/local/bin:${PATH}"
ENV PYTHONPATH=/usr/local/lib/python3.13/dist-packages
ENV VAPOURSYNTH_EXTRA_PLUGIN_PATH=/usr/local/lib/vapoursynth
ENV XDG_CONFIG_HOME=/etc/xdg
RUN x265 --version && \
    ffmpeg -hide_banner -encoders | grep -q libx265
RUN printf '%s\n' \
    '#!/bin/sh' \
    'exec python3 -m vapoursynth "$@"' \
    > /usr/local/bin/vapoursynth && \
    chmod +x /usr/local/bin/vapoursynth && \
    python3 -m vapoursynth config
RUN mkdir -p /etc/OpenCL/vendors && \
    echo "libnvidia-opencl.so.1" > /etc/OpenCL/vendors/nvidia.icd
ENV NVIDIA_VISIBLE_DEVICES=all
ENV NVIDIA_DRIVER_CAPABILITIES=compute,utility
WORKDIR /video
RUN (getent group "$GID" > /dev/null || groupadd --gid "$GID" sav1n) && \
    useradd --non-unique --uid "$UID" --gid "$GID" --create-home --shell /bin/bash sav1n && \
    chown "$UID:$GID" /video
ENV HOME=/home/sav1n
USER sav1n
ENTRYPOINT ["sav1n"]
