# NVDEC Interface Headers

Unmodified interface declarations from FFmpeg/nv-codec-headers release
`n13.0.19.0`, source commit `e844e5b26f46bb77479f063029595293aa8f812d`:

https://github.com/FFmpeg/nv-codec-headers/tree/e844e5b26f46bb77479f063029595293aa8f812d/include/ffnvcodec

Only `dynlink_cuda.h`, `dynlink_cuviddec.h`, and `dynlink_nvcuvid.h` are used.
Their complete license notices are preserved in each header. They describe the
installed NVIDIA driver APIs; no CUDA toolkit or NVIDIA binaries are bundled.

The client loads `nvcuda.dll` and `nvcuvid.dll` only from Windows System32.
Absent or unsupported drivers leave the software decoder available.

SHA-256:

```
2c74cf9b0ace0503ca18d2c43645d6cb1ec0dd7ea7163f7880bba5141586f275  dynlink_cuda.h
1244165ea5521d4be865995eb1c49b8910f755d0ca35004e1c4010ef7f612143  dynlink_cuviddec.h
2141dade75f3178520092dce2c02df9b47c4b19036b56bb9316bd83a07bf16f1  dynlink_nvcuvid.h
```