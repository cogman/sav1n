from vapoursynth import core
import vapoursynth as vs

clip = core.lsmas.LWLibavSource('ToS-4k-1920.mov')

clip = clip.std.Trim(first=1000, last=2000)
clip.set_output()