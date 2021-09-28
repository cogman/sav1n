from vapoursynth import core
import vapoursynth as vs

clip = core.lsmas.LWLibavSource(file)

clip = clip.std.Trim(first=1000, last=1500)
clip.set_output()