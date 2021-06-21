from vapoursynth import core
import vapoursynth as vs

core.std.BlankClip(format=vs.YUV420P10,length=1000).set_output()
