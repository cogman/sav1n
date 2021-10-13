FROM debian:bullseye-slim AS runtime

ENV LD_LIBRARY_PATH=$LD_LIBRARY_PATH:/usr/local/lib:/usr/local/lib/x86_64-linux-gnu/:/usr/local/lib/vapoursynth

RUN apt-get update && apt-get install -y \
    alien \
    clinfo \
    libass9 \
    libfftw3-bin \
    libpython3.9 \
    libvdpau1 \
    libva2 \
    libva-drm2 \
    libxcb1 \
    mkvtoolnix \
    ocl-icd-libopencl1 \
    python3 \
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
RUN apt-get update && apt-get install -y \
    autoconf \
    automake \
    build-essential \
    cmake \
    git \
    git-core \
    libass-dev \
    libboost-dev \
    libboost-filesystem-dev \
    libfftw3-dev \
    libfreetype6-dev \
    libtool \
    libva-dev \
    libvdpau-dev \
    nasm \
    ninja-build \
    ocl-icd-opencl-dev \
    perl \
    pkg-config \
    python3-pip \
    texinfo \
    wget \
    yasm \
    zlib1g-dev \
    && rm -rf /var/lib/apt/lists/*

RUN pip3 --no-cache-dir install meson setuptools cython sphinx

ARG CFLAGS="-O3 -march=znver1 -fPIC"
ARG CXXFLAGS="-O3 -march=znver1 -fPIC"

FROM build AS vapoursynth
RUN mkdir -p /vapoursynth/dependencies && git clone https://github.com/sekrit-twc/zimg -b master --depth=1 /vapoursynth/dependencies/zimg
WORKDIR /vapoursynth/dependencies/zimg
RUN ./autogen.sh  && \
    ./configure --enable-x86simd --disable-static --enable-shared --with-plugindir=/usr/local/lib/vapoursynth && \
    make && \
    make install

RUN git clone https://github.com/vapoursynth/vapoursynth.git --depth=1 -b R57 /vapoursynth/build
WORKDIR /vapoursynth/build
RUN ./autogen.sh && \
    ./configure --enable-shared && \
    make && \
    make install

FROM vapoursynth AS miscFilters
RUN git clone https://github.com/vapoursynth/vs-miscfilters-obsolete.git -b master /vs-misc
WORKDIR /vs-misc
RUN meson build && \
    ninja -C build

FROM vapoursynth AS vivtc
RUN git clone https://github.com/vapoursynth/vivtc.git -b master /vivtc
WORKDIR /vivtc
RUN meson build && \
    ninja -C build

FROM vapoursynth AS addGrain
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

FROM vapoursynth AS ctmf
RUN git clone https://github.com/HomeOfVapourSynthEvolution/VapourSynth-CTMF.git --depth=1 -b master /ctmf
WORKDIR /ctmf
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

FROM vapoursynth AS dfttest
RUN git clone https://github.com/HomeOfVapourSynthEvolution/VapourSynth-DFTTest.git --depth=1 -b master /dfttest
WORKDIR /dfttest
RUN meson build && \
    ninja -C build && \
    ninja -C build install

FROM vapoursynth AS eedi2
RUN git clone https://github.com/HomeOfVapourSynthEvolution/VapourSynth-EEDI2.git --depth=1 -b master /eedi2
WORKDIR /eedi2
RUN meson build && \
    ninja -C build && \
    ninja -C build install

FROM vapoursynth AS eedi3
RUN git clone https://github.com/HomeOfVapourSynthEvolution/VapourSynth-EEDI3.git --depth=1 -b master /eedi3
WORKDIR /eedi3
RUN meson build && \
    ninja -C build && \
    ninja -C build install

FROM vapoursynth AS fft3dfilter
RUN git clone https://github.com/myrsloik/VapourSynth-FFT3DFilter.git --depth=1 -b master /fft3dfilter
WORKDIR /fft3dfilter
RUN meson build && \
    ninja -C build && \
    ninja -C build install

FROM vapoursynth AS fluxsmooth
RUN git clone https://github.com/dubhater/vapoursynth-fluxsmooth.git --depth=1 -b master /fluxsmooth
WORKDIR /fluxsmooth
RUN ./autogen.sh && \
    ./configure && \
    make

FROM vapoursynth AS fmtconv
RUN git clone https://github.com/EleonoreMizo/fmtconv.git --depth=1 -b master /fmtconv
WORKDIR /fmtconv/build/unix
RUN ./autogen.sh && \
    ./configure && \
    make

FROM vapoursynth AS hqdn3d
RUN git clone https://github.com/Hinterwaeldlers/vapoursynth-hqdn3d.git --depth=1 -b master /hqdn3d
WORKDIR /hqdn3d
RUN ./autogen.sh && \
    ./configure && \
    make

FROM vapoursynth AS knlmeanscl
RUN git clone https://github.com/Khanattila/KNLMeansCL.git --depth=1 -b master /knlmeanscl
WORKDIR /knlmeanscl
RUN meson build && \
    ninja -C build && \
    ninja -C build install

FROM vapoursynth AS mvtools
RUN git clone https://github.com/dubhater/vapoursynth-mvtools.git --depth=1 -b master /mvtools
WORKDIR /mvtools
RUN meson build && \
    ninja -C build

FROM vapoursynth AS nnedi3
RUN git clone https://github.com/dubhater/vapoursynth-nnedi3.git --depth=1 -b master /nnedi3
WORKDIR /nnedi3
RUN ./autogen.sh && \
    ./configure && \
    make

FROM vapoursynth AS nnedi3cl
RUN git clone https://github.com/HomeOfVapourSynthEvolution/VapourSynth-NNEDI3CL.git --depth=1 -b master /nnedi3cl
WORKDIR /nnedi3cl
RUN meson build && \
    ninja -C build

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
RUN git clone --recursive https://github.com/sekrit-twc/znedi3 /znedi3
WORKDIR /znedi3
RUN make

FROM vapoursynth AS HAvsFunc
RUN git clone https://github.com/dubhater/vapoursynth-adjust.git --depth=1 -b master /adjust
RUN mv /adjust/adjust.py /usr/local/lib/python3.9/site-packages

RUN git clone https://github.com/AmusementClub/mvsfunc.git --depth=1 -b mod /mvsfunc
RUN mv /mvsfunc/mvsfunc.py /usr/local/lib/python3.9/site-packages

RUN git clone https://github.com/mawen1250/VapourSynth-script.git --depth=1 -b master /nnedi3_resample
RUN mv /nnedi3_resample/nnedi3_resample.py /usr/local/lib/python3.9/site-packages

RUN git clone https://github.com/cogman/havsfunc.git --depth=1 -b master /havsfunc
RUN mv /havsfunc/havsfunc.py /usr/local/lib/python3.9/site-packages

COPY --from=addGrain /usr/local/lib/vapoursynth /usr/local/lib/vapoursynth
COPY --from=vcas /usr/local/lib/vapoursynth /usr/local/lib/vapoursynth
COPY --from=ctmf /usr/local/lib/vapoursynth /usr/local/lib/vapoursynth
COPY --from=dct /usr/local/lib/vapoursynth /usr/local/lib/vapoursynth
COPY --from=deblock /usr/local/lib/vapoursynth /usr/local/lib/vapoursynth
COPY --from=dfttest /usr/local/lib/vapoursynth /usr/local/lib/vapoursynth
COPY --from=eedi2 /usr/local/lib/vapoursynth /usr/local/lib/vapoursynth
COPY --from=eedi3 /usr/local/lib/vapoursynth /usr/local/lib/vapoursynth
COPY --from=fft3dfilter /usr/local/lib/vapoursynth /usr/local/lib/vapoursynth
COPY --from=fluxsmooth /fluxsmooth/.libs/*.so /usr/local/lib/vapoursynth
COPY --from=fmtconv /fmtconv/build/unix/.libs/*.so /usr/local/lib/vapoursynth
COPY --from=hqdn3d /hqdn3d/.libs/*.so /usr/local/lib/vapoursynth
COPY --from=knlmeanscl /knlmeanscl/build/*.so /usr/local/lib/vapoursynth
COPY --from=mvtools /mvtools/build/*.so /usr/local/lib/vapoursynth
COPY --from=nnedi3 /nnedi3/.libs/*.so /usr/local/lib/vapoursynth
COPY --from=nnedi3cl /nnedi3cl/build/*.so /usr/local/lib/vapoursynth
COPY --from=sangnom /sangnom/build/*.so /usr/local/lib/vapoursynth
COPY --from=ttempsmooth /ttempsmooth/build/*.so /usr/local/lib/vapoursynth
COPY --from=znedi3 /znedi3/*.so /usr/local/lib/vapoursynth
COPY --from=znedi3 /znedi3/nnedi3_weights.bin /usr/local/lib/vapoursynth

FROM build AS aom
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

COPY --from=vapoursynth /usr/local/include /usr/local/include
COPY --from=vapoursynth /usr/local/lib /usr/local/lib

COPY --from=vmaf /usr/local/include /usr/local/include
COPY --from=vmaf /usr/local/lib /usr/local/lib

COPY --from=vpx /usr/local/include /usr/local/include
COPY --from=vpx /usr/local/lib /usr/local/lib

WORKDIR /ffmpeg
RUN git clone --branch 'release/4.4' https://github.com/FFmpeg/FFmpeg.git --depth 1 ffmpeg  && \
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
RUN git clone https://github.com/l-smash/l-smash --depth 1 /lsmash
WORKDIR /lsmash
RUN ./configure --enable-shared && \
    make && \
    make install

RUN git clone https://github.com/HolyWu/L-SMASH-Works.git --depth 1 /lsmash-plugin && mkdir -p /lsmash-plugin/build-vapoursynth /lsmash-plugin/build-avisynth
WORKDIR /lsmash-plugin/build-vapoursynth
RUN meson "../VapourSynth" && \
    ninja && \
    ninja install

FROM runtime
WORKDIR /sav1n

RUN rm -rf /var/lib/apt/lists/*

COPY --from=vapoursynth /usr/local/lib/*.so* /usr/local/lib/
#COPY --from=vapoursynth /usr/local/lib/vapoursynth /usr/local/lib/vapoursynth
COPY --from=vapoursynth /usr/local/lib/python3.9/site-packages /usr/local/lib/python3.9/site-packages

COPY --from=vapoursynth /usr/local/bin/vspipe /usr/local/bin/
COPY --from=vpx /usr/local/bin/vpxenc /usr/local/bin/
COPY --from=vmaf /vmaf/vmaf-2.1.1/model/vmaf_v0.6.1.json /usr/local/share/model/
COPY --from=aom /usr/local/bin/aomenc /usr/local/bin/
COPY --from=ffmpeg /usr/local/bin/ffmpeg /usr/local/bin/
COPY --from=ffmpeg /usr/local/bin/ffprobe /usr/local/bin/
COPY --from=ffmpeg /usr/local/lib/*.so* /usr/local/lib/
COPY --from=ffmpeg /usr/local/lib/x86_64-linux-gnu/*.so* /usr/local/lib/x86_64-linux-gnu/
COPY --from=ffms2 /ffms2/src/core/.libs/libffms2.so /usr/local/lib/python3.9/site-packages/
COPY --from=lsmash /usr/local/lib/*.so* /usr/local/lib/
COPY --from=lsmash /usr/local/lib/vapoursynth /usr/local/lib/vapoursynth/
COPY --from=HAvsFunc /usr/local/lib/python3.9/site-packages/*.py /usr/local/lib/python3.9/site-packages/
COPY --from=HAvsFunc /usr/local/lib/vapoursynth /usr/local/lib/vapoursynth/
COPY --from=nnedi3 /nnedi3/src/nnedi3_weights.bin /usr/local/share/nnedi3/
COPY --from=miscFilters /vs-misc/build/libmiscfilters.so /usr/local/lib/vapoursynth/
COPY --from=vivtc /vivtc/build/*.so /usr/local/lib/vapoursynth/

COPY --from=rustBuild /sav1n/target/release/sav1n .

ENV PATH="/sav1n:/usr/local/bin:${PATH}"
ENV PYTHONPATH=/usr/local/lib/python3.9/site-packages
RUN mkdir -p /etc/OpenCL/vendors && \
    echo "libnvidia-opencl.so.1" > /etc/OpenCL/vendors/nvidia.icd
ENV NVIDIA_VISIBLE_DEVICES=all
ENV NVIDIA_DRIVER_CAPABILITIES=compute,utility
WORKDIR /video
ENTRYPOINT ["sav1n"]
