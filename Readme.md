[![codecov](https://codecov.io/gh/cogman/sav1n/branch/main/graph/badge.svg?token=GGPZGZQPO3)](https://codecov.io/gh/cogman/sav1n)
![rust build](https://github.com/cogman/sav1n/actions/workflows/rust.yml/badge.svg)
![docker build](https://github.com/cogman/sav1n/actions/workflows/docker-image.yml/badge.svg)

# Sav1n

A progressive Vapoursynth encoding system.

Sav1n attempts to do a single Vapoursynth pass over encoded media. This allows for aggressively slow vapoursynth actions.
You can use QTGMC or KNLMeans while keeping your encoder CPU hot.

Sav1n applies scene detection and perceptual quality estimation to give automatic per-scene quality adjustments. Choose VMAF (the default), ColorVideoVDP (CVVDP), or Butteraugli as the quality target.

## Usage

The easiest way to start using sav1n is through docker:

```bash
docker run -v `pwd`:/video -it --rm cogman/sav1n:latest -i test.mkv -v test.vpy
```

The image runs as an unprivileged user, so files written to `/video` are owned by that user instead of root.
The UID and GID default to `1000`. To match a different host user, override them at runtime:

```bash
docker run --user $(id -u):$(id -g) -v `pwd`:/video -it --rm cogman/sav1n:latest -i test.mkv -v test.vpy
```

or when building the image:

```bash
docker build --build-arg UID=$(id -u) --build-arg GID=$(id -g) -t sav1n .
```

Sending `SIGINT` (Ctrl-C) or `SIGTERM` stops all child encoders, gives them a short grace period, and removes temporary folders before exiting.

## Quality metrics

The standard Docker image includes all three metric tools. Outside Docker, install FFmpeg/ffprobe (with `libvmaf` for VMAF), the official `cvvdp` Python package, and JPEG XL's `butteraugli_main` tool as needed. The VMAF model is expected at `/usr/local/share/model/vmaf_v0.6.1.json`.

```bash
# Existing VMAF behavior: default target 95, tolerance 0.5 VMAF points.
sav1n -i input.mkv -v script.vpy --metric vmaf --target 95

# CVVDP target in JOD; higher is better.
sav1n -i input.mkv -v script.vpy --metric cvvdp --target 8 \
  --cvvdp-display standard_fhd --cvvdp-device cpu

# Mean Butteraugli distance across every scene frame; lower is better.
sav1n -i input.mkv -v script.vpy --metric butteraugli --target 1.5
```

These targets are examples, **not equivalent quality levels**. CVVDP and Butteraugli require an explicit `--target`. The search chooses the highest CQ/CRF that meets the selected metric's threshold; if no tested value qualifies, it falls back to the minimum CQ. Search convergence alone does not guarantee the target was met.

- `--tolerance` is in native metric units: defaults are `0.5` for VMAF and `0.05` for CVVDP/Butteraugli. Use `0` to fully resolve the CQ bracket within the evaluation budget.
- `--trial-cpu-used` controls the trial **encoder's speed**, not metric CPU usage. It defaults to `3`; initial probes retain their existing speed of `6`. `--cpu_used` controls final encoding speed.
- Existing `-t`, `--vmaf_target`, and `--vmaf_cpu_used` remain supported. The long VMAF-specific names are aliases for the generic settings.
- CVVDP defaults to CPU and the `standard_fhd` display model. Display/viewing conditions affect its predictions; choose an appropriate display model, especially for HDR. The bundled PyTorch build is CPU-only; GPU use requires a compatible external PyTorch installation.
- Butteraugli compares every matching frame pair, then takes the arithmetic mean of the distances. It is a spatial image metric, not a temporal video metric. Frame preparation uses FFmpeg's `zscale` filter and preserves 16-bit RGB precision. Only BT.709/sRGB SDR is supported; incompatible explicit color tags are rejected. Untagged YUV is assumed to be limited-range BT.709, and untagged RGB full-range sRGB. The VapourSynth Y4M-to-FFV1 path can lose color metadata, so HDR or full-range input may not be detected: ensure the processed output matches these assumptions before using Butteraugli.

All metrics compare trial encodes against the **VapourSynth-processed reference scene**. Trial output and backend artifacts use isolated temporary workspaces, which require additional disk space; CVVDP also keeps a lossless retimed copy of each scene reference while that scene is searched. The final scene is encoded again at the selected CQ and final speed; its quality is not separately measured.
