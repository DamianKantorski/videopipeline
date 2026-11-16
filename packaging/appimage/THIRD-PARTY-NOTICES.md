# Dołączone składniki (AppImage, deb, Windows .zip)

| Składnik | Licencja | Źródło |
|---|---|---|
| VideoPipeline | GPL-3.0 | https://github.com/8brzrs26ws-cloud/videopipeline |
| FFmpeg (biblioteki dynamiczne libavformat, libavcodec, libavfilter, libavutil, libswscale, libswresample — program wywołuje je bezpośrednio; budowany z `--enable-gpl` — filtr hqdn3d jest na licencji GPL — bez `--enable-nonfree`; z ffnvcodec, NVENC/NVDEC, CUDA-LLVM, dav1d; Linux: VAAPI i libvpl, Windows: libvpl, D3D11VA, DXVA2) | GPL-2.0+ | https://ffmpeg.org/download.html |
| nv-codec-headers (statycznie, tylko nagłówki) | MIT | https://github.com/FFmpeg/nv-codec-headers |
| libva, libdrm (Linux; dynamicznie — kopie w AppImage używane tylko, gdy brak ich w systemie) | MIT | https://github.com/intel/libva |
| libvpl / oneVPL dispatcher (Linux: dynamicznie, Windows: statycznie w bibliotekach FFmpeg) | MIT | https://github.com/intel/libvpl |
| zimg (filtr zscale w libavfilter — mapowanie tonów HDR → SDR; Linux: biblioteka z Debiana, Windows: statycznie) | WTFPL | https://github.com/sekrit-twc/zimg |
| OpenCL ICD Loader (tonemap_opencl w libavfilter; Linux: ocl-icd z Debiana, Windows: ładowarka Khronos statycznie) | Apache-2.0 / BSD-2-Clause | https://github.com/KhronosGroup/OpenCL-ICD-Loader |
| dav1d (Linux: biblioteka z Debiana, Windows: statycznie w bibliotekach FFmpeg) | BSD-2-Clause | https://code.videolan.org/videolan/dav1d |
| MediaInfoLib (biblioteka MediaInfo — okienko F2; Linux: libmediainfo i libzen z dystrybucji Debian z zależnościami, Windows: oficjalna MediaInfo.dll MediaArea) | BSD-2-Clause (libzen: zlib) | https://mediaarea.net/MediaInfo |
| libplacebo (statycznie w libavfilter — filtr libplacebo; budowana ze źródeł) | LGPL-2.1+ | https://code.videolan.org/videolan/libplacebo |
| glslang (Windows: statycznie w bibliotekach FFmpeg, kompilator shaderów libplacebo) | BSD-3-Clause / BSD-2-Clause / MIT / Apache-2.0 | https://github.com/KhronosGroup/glslang |
| shaderc (Linux: kompilator shaderów libplacebo, z Debiana) | Apache-2.0 | https://github.com/google/shaderc |
| Vulkan-Headers (nagłówki; ffmpeg ładuje sterownik Vulkan w trakcie działania) | Apache-2.0 / MIT | https://github.com/KhronosGroup/Vulkan-Headers |
| Alacritty (terminal `videopipeline-term`, budowany ze źródeł z podmienioną ikoną) | Apache-2.0 (LICENSE-ALACRITTY) | https://github.com/alacritty/alacritty |
| dolby_vision, hdr10plus (biblioteki Rust w programie — odczyt i wstrzykiwanie DV/HDR10+) | MIT | https://github.com/quietvoid/dovi_tool, https://github.com/quietvoid/hdr10plus_tool |
| Biblioteka standardowa Rusta (libstd / std-*.dll) | MIT / Apache-2.0 | https://github.com/rust-lang/rust |
| Windows: środowisko uruchomieniowe MinGW-w64 (libgcc, libstdc++, winpthreads — statycznie lub jako DLL) | GPL-3.0 z wyjątkiem GCC Runtime Library / MIT / ZPL | https://www.mingw-w64.org |

Źródła pakietów Debiana (libmediainfo i jej zależności) są dostępne przez `apt-get source <pakiet>`
albo na https://packages.debian.org. Wersje użyte w danym wydaniu są wypisywane na początku
logu budowania (`./build-local.sh`).

Sterowniki GPU (NVIDIA: libcuda / nvcuda.dll, NVENC, nvidia-smi; Intel: iHD, oneVPL GPU runtime)
NIE są dołączone — program używa tych z systemu.
