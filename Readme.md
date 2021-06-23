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