from vapoursynth import core
import vapoursynth as vs

print(core.std.LoadPlugin('libknlmeanscl.so'))

clip = core.lsmas.LWLibavSource(file)

clip = clip.std.Trim(first=1000, last=1500)

print(dir(clip))
clip = clip.knlm.KNLMeansCL(d=1, a=1, s=1, h=1)
clip.set_output()
