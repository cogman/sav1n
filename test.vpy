from vapoursynth import core
import vapoursynth as vs

# Smoke test for test.mkv: 5s, 320x160, 24fps (120 frames), 8-bit 4:2:0 H.264.
clip = core.lsmas.LWLibavSource(file)

# sav1n encodes at 10-bit, and libvmaf needs the reference y4m in the same
# pixel format as the decoded 10-bit encode.
clip = core.resize.Point(clip, format=vs.YUV420P10)

clip.set_output()
