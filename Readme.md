[![codecov](https://codecov.io/gh/cogman/sav1n/branch/main/graph/badge.svg?token=GGPZGZQPO3)](https://codecov.io/gh/cogman/sav1n)
![rust build](https://github.com/cogman/sav1n/actions/workflows/rust.yml/badge.svg)
![docker build](https://github.com/cogman/sav1n/actions/workflows/docker-image.yml/badge.svg)

# Sav1n

A progressive Vapoursynth encoding system.

Sav1n attempts to do a single Vapoursynth pass over encoded media. This allows for aggressively slow vapoursynth actions.
You can use QTGMC or KNLMeans while keeping your encoder CPU hot.

Sav1n applies scene detection and VMAF quality estimations to give automatic per-scene quality adjustments.

## Usage

The easiest way to start using sav1n is through docker:

```bash
docker run -v `pwd`:/video -it --rm cogman/sav1n:latest -i test.vpy
```

The image runs as an unprivileged user, so files written to `/video` are owned by that user instead of root.
The UID and GID default to `1000`. To match a different host user, override them at runtime:

```bash
docker run --user $(id -u):$(id -g) -v `pwd`:/video -it --rm cogman/sav1n:latest -i test.vpy
```

or when building the image:

```bash
docker build --build-arg UID=$(id -u) --build-arg GID=$(id -g) -t sav1n .
```

Sending `SIGINT` (Ctrl-C) or `SIGTERM` stops all child encoders, gives them a short grace period, and removes temporary folders before exiting.
