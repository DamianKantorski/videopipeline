use crate::{
    analysis::Analysis,
    config::*,
    model::{read_progress, Panel, State},
    probe,
    proc,
};
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

struct Running {
    /// Wątek silnika (biblioteki FFmpeg) kodujący ten plik.
    job: thread::JoinHandle<anyhow::Result<crate::av::Report>>,
    name: String,
    live: bool,
    /// Plik zapisywany w trakcie: <wynik>.part.
    output: PathBuf,
    /// Docelowa nazwa pliku wynikowego (po sukcesie .part → wynik).
    final_out: PathBuf,
    pf: PathBuf,
}

fn sv(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect()
}

/// Wyjście ffmpeg: mapowanie strumieni (przed opcjami kodera) i końcówka z plikiem.
struct OutSpec {
    streams: Vec<String>,
    tail: Vec<String>,
}

/// Opcje kodera wideo tylko dla strumienia wideo (`-preset` → `-preset:v`) — przy kodowaniu wprost
/// do kontenera w tym samym poleceniu są też strumienie audio (kopia / E-AC-3).
fn scope_v(args: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut opt = true; // na przemian: opcja, wartość
    for a in args {
        if opt && a.starts_with('-') && !a.contains(':') {
            out.push(format!("{a}:v"));
        } else {
            out.push(a);
        }
        opt = !opt;
    }
    out
}

/// Tryb bezpośredni: wideo + audio (kopia lub E-AC-3 w locie) + napisy od razu w pliku wynikowym.
fn direct_out(opts: &Opts, a: &Analysis, p: &Paths, part: &Path) -> OutSpec {
    let info = probe::stream_codecs(&a.input_file);
    let mut streams = sv(&["-map", "0:v:0"]);
    let (src, _) = crate::mux::source_streams(opts, a, p.container, 0, &info);
    streams.extend(src);
    let mut tail = crate::mux::meta_args(opts, a, 0);
    tail.extend(sv(&["-map_chapters", "0"]));
    match p.container {
        Container::Mp4 => {
            tail.extend(sv(&["-movflags", "+faststart"]));
            if opts.codec == Codec::Hevc {
                tail.extend(sv(&["-tag:v", "hvc1"])); // zgodność z odtwarzaczami (Apple, przeglądarki)
            }
            tail.extend(sv(&["-f", "mp4"]));
        }
        Container::Mkv => tail.extend(sv(&["-f", "matroska"])),
    }
    tail.extend(sv(&["-y"]));
    tail.push(part.to_string_lossy().into());
    OutSpec { streams, tail }
}

/// Mapowanie tonów HDR (PQ/HLG, BT.2020) → SDR BT.709 8-bit dla AVC. Krzywa Hable bez desaturacji.
/// CPU: zscale (libzimg) liniowo → tonemap → BT.709.
pub(crate) const TONEMAP_CPU: &str = "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,\
tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuv420p";
/// GPU przez OpenCL (NVIDIA / Intel): klatki z pamięci → karta → z powrotem (koder NVENC bierze z pamięci).
pub(crate) const TONEMAP_OPENCL: &str = "format=p010le,hwupload,tonemap_opencl=tonemap=hable:desat=0:\
t=bt709:m=bt709:p=bt709:r=tv:format=nv12,hwdownload,format=nv12";
/// libplacebo (Vulkan): mapowanie tonów jak w mpv (gpu-next) — krzywa „auto” (spline), mapowanie gamy
/// perceptualne, wykrywanie szczytowej jasności, odzyskiwanie kontrastu, RPU Dolby Vision i HDR10+
/// z klatek źródła, dithering przy przejściu do 8 bitów. Przy skalowaniu do 1080p zmniejsza też obraz
/// (EWA Lanczos) — w jednym przejściu na karcie. Klatki z pamięci i do pamięci (koder bierze z RAM).
pub(crate) fn tonemap_placebo(scale: Option<&Scale>, format: &str) -> String {
    let size = scale.map(|s| format!("w={}:h={}:downscaler=ewa_lanczos:", s.dst.0, s.dst.1)).unwrap_or_default();
    format!(
        "libplacebo={size}format={format}:colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=tv:\
tonemapping=auto:gamut_mode=perceptual:peak_detect=1:contrast_recovery=0.3:apply_dolbyvision=1:dithering=blue"
    )
}

/// Urządzenie Vulkan dla libplacebo (nazwa karty, np. "NVIDIA", albo numer).
pub(crate) fn vulkan_args(dev: &str) -> Vec<String> {
    vec!["-init_hw_device".into(), format!("vulkan=vk:{dev}"), "-filter_hw_device".into(), "vk".into()]
}

/// Intel QSV: opcje vpp_qsv (sprzętowe mapowanie tonów z metadanych HDR źródła).
pub(crate) const TONEMAP_QSV: &str = "tonemap=1:out_color_matrix=bt709:out_color_primaries=bt709:out_color_transfer=bt709";
/// Intel VAAPI: filtr po hwupload / dekodowaniu VAAPI.
pub(crate) const TONEMAP_VAAPI: &str = "tonemap_vaapi=format=nv12:t=bt709:m=bt709:p=bt709";

/// Znaczniki koloru wyniku SDR: jak źródło, a po mapowaniu tonów — BT.709.
fn sdr_tags(c: &crate::probe::ColorProfile, tonemapped: bool) -> Vec<String> {
    if tonemapped {
        sv(&["-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709", "-color_range", "tv"])
    } else {
        sv(&[
            "-color_primaries", &c.sdr_primaries, "-color_trc", &c.sdr_trc,
            "-colorspace", &c.sdr_space, "-color_range", &c.sdr_range,
        ])
    }
}

fn build_args_nvenc(
    opts: &Opts,
    rt: &Runtime,
    a: &Analysis,
    out: &OutSpec,
    pf: &Path,
) -> (Vec<String>, String) {
    let c = &a.color;
    let mut hw: Vec<String> = vec![];
    let mut fmt: Vec<String> = vec![];
    let mut filt: Vec<String> = vec![];
    let note;
    // Kadrowanie i skalowanie na CPU: NVENC i tak dostaje klatki z pamięci (dekodowanie CUDA bez
    // -hwaccel_output_format). Skalowanie przed odszumianiem / mapowaniem tonów — mniej pikseli dalej.
    let scale = scale_target(opts, a);
    // Pełny zakres (aparaty) → ograniczony; przy mapowaniu tonów robi to filtr mapujący.
    let range_fix = a.color.full_range() && !(opts.codec == Codec::Avc && a.color.is_hdr());
    let pre: Vec<String> = a.crop.iter().cloned().chain(cpu_scale_range(scale.as_ref(), range_fix)).collect();

    // AVC jest zawsze 8-bit SDR (źródła HDR odrzuca analiza; 10-bit SDR jest przeliczane do 8 bitów).
    let avc = opts.codec == Codec::Avc;
    if c.is_sdr_8bit || avc {
        match opts.codec {
            Codec::Hevc => fmt.extend(sv(&["-profile:v", "main"])),
            Codec::Avc => fmt.extend(sv(&["-profile:v", "high"])),
            Codec::Av1 => {}
        }
        // AVC ze źródła HDR: mapowanie tonów do SDR BT.709 (OpenCL na karcie albo CPU).
        let tonemap = avc && c.is_hdr();
        fmt.extend(sv(&["-pix_fmt", "yuv420p"]));
        fmt.extend(sdr_tags(c, tonemap));

        if opts.denoise && c.is_sdr_8bit && rt.denoise != Denoise::Off {
            let mut chain: Vec<String> = pre.clone();
            if rt.denoise == Denoise::Cuda {
                chain.push(format!(
                    "format=yuv420p,hwupload_cuda,{DENOISE_CUDA_FILTER},hwdownload,format=yuv420p"
                ));
                hw = sv(&["-init_hw_device", "cuda=denoise:0", "-filter_hw_device", "denoise"]);
                note = "SDR 8-bit + CUDA denoise";
            } else {
                chain.push(DENOISE_CPU_FILTER.into());
                note = "SDR 8-bit + CPU denoise";
            }
            filt = vec!["-vf".into(), chain.join(",")];
        } else if tonemap && rt.tonemap == Tonemap::Placebo {
            // Skalowanie robi libplacebo (na karcie, razem z mapowaniem tonów) — bez skalowania na CPU.
            let mut chain: Vec<String> = a.crop.iter().cloned().collect();
            chain.push(tonemap_placebo(scale.as_ref(), "yuv420p"));
            hw = vulkan_args(&rt.vulkan_dev);
            note = "HDR -> SDR AVC 8-bit (tone mapping libplacebo)";
            filt = vec!["-vf".into(), chain.join(",")];
        } else if tonemap {
            let mut chain: Vec<String> = pre.clone();
            if rt.tonemap == Tonemap::OpenCl {
                hw = sv(&["-init_hw_device", "opencl=ocl", "-filter_hw_device", "ocl"]);
                chain.push(TONEMAP_OPENCL.into());
                note = "HDR -> SDR AVC 8-bit (tone mapping OpenCL)";
            } else {
                chain.push(TONEMAP_CPU.into());
                note = "HDR -> SDR AVC 8-bit (tone mapping CPU)";
            }
            filt = vec!["-vf".into(), chain.join(",")];
        } else {
            if !pre.is_empty() {
                filt = vec!["-vf".into(), pre.join(",")];
            }
            note = match opts.codec {
                Codec::Av1 => "SDR 8-bit -> AV1 10-bit (highbitdepth)",
                Codec::Avc if !c.is_sdr_8bit => "SDR 10-bit -> AVC 8-bit (High)",
                Codec::Avc => "SDR 8-bit -> AVC High",
                Codec::Hevc => "SDR 8-bit",
            };
        }
    } else {
        if opts.codec == Codec::Hevc {
            fmt.extend(sv(&["-profile:v", "main10"]));
        }
        fmt.extend(sv(&["-pix_fmt", "p010le", "-color_primaries", "bt2020"]));
        fmt.extend(sv(&["-color_trc", "smpte2084", "-colorspace", "bt2020nc"]));
        if !pre.is_empty() {
            filt = vec!["-vf".into(), pre.join(",")];
        }
        note = if opts.denoise {
            "HDR — denoise pominięty (tylko SDR 8-bit)"
        } else {
            "HDR 10-bit"
        };
    }

    let cq = opts.cq().to_string();
    let enc: Vec<String> = match opts.codec {
        Codec::Av1 => sv(&[
            "-c:v", "av1_nvenc", "-preset", "p7", "-tune", &rt.av1_tune, "-rc", "vbr",
            "-cq", &cq, "-b:v", "0", "-spatial-aq", "1", "-temporal-aq", "1", "-bf", "4",
            "-b_ref_mode", "middle", "-multipass", "fullres", "-rc-lookahead", "32",
            "-highbitdepth", "1",
        ]),
        Codec::Hevc => sv(&[
            "-c:v", "hevc_nvenc", "-preset", "p7", "-tune", "hq", "-rc", "vbr", "-cq",
            &cq, "-b:v", "0", "-spatial-aq", "1", "-temporal-aq", "1", "-b_ref_mode",
            "middle", "-multipass", "2", "-rc-lookahead", "32",
        ]),
        // AVC: te same priorytety jakości co HEVC (p7/hq, VBR CQ, AQ, B-klatki referencyjne,
        // pełne multipass, lookahead) + CABAC.
        Codec::Avc => sv(&[
            "-c:v", "h264_nvenc", "-preset", "p7", "-tune", "hq", "-rc", "vbr", "-cq", &cq,
            "-b:v", "0", "-spatial-aq", "1", "-temporal-aq", "1", "-bf", "3", "-b_ref_mode",
            "middle", "-multipass", "fullres", "-rc-lookahead", "32", "-coder", "cabac",
        ]),
    };

    let mut v = sv(&["-hide_banner", "-nostats", "-progress"]);
    v.push(pf.to_string_lossy().into());
    v.extend(hw);
    // Oba równoległe enkodery dekodują przez CUDA/NVDEC.
    v.extend(sv(&["-hwaccel", "cuda"]));
    v.push("-i".into());
    v.push(a.input_file.to_string_lossy().into());
    v.extend(filt);
    v.extend(out.streams.iter().cloned());
    v.extend(scope_v(enc));
    v.extend(scope_v(fmt));
    v.extend(out.tail.iter().cloned());
    let mut note = match &scale {
        Some(s) => format!("{note} · skalowanie {}x{} → {}x{}", s.src.0, s.src.1, s.dst.0, s.dst.1),
        None => note.to_string(),
    };
    if range_fix {
        note.push_str(" · pełny zakres → TV");
    }
    (v, note)
}


/// Skalowanie do 1080p: (szerokość, wysokość) po kadrowaniu i docelowe — tylko gdy obraz jest
/// większy niż 1920×1080. Proporcje zachowane, wymiary parzyste (4:2:0).
pub(crate) struct Scale {
    pub src: (u32, u32),
    pub dst: (u32, u32),
}

impl Scale {
    /// Filtr CPU (swscale, Lanczos) — po crop, przed odszumianiem i mapowaniem tonów.
    fn cpu(&self) -> String {
        format!("scale={}:{}:flags=lanczos", self.dst.0, self.dst.1)
    }
    /// Współczynniki dla przesunięć obszaru aktywnego (L5) w RPU Dolby Vision.
    pub fn factors(&self) -> (f64, f64) {
        (self.dst.0 as f64 / self.src.0 as f64, self.dst.1 as f64 / self.src.1 as f64)
    }
}

fn fit_1080(w: u32, h: u32) -> Option<(u32, u32)> {
    if w <= 1920 && h <= 1080 {
        return None;
    }
    let f = (1920.0 / w as f64).min(1080.0 / h as f64);
    let even = |x: f64| (((x / 2.0).round() as u32) * 2).max(2);
    Some((even(w as f64 * f), even(h as f64 * f)))
}

pub(crate) fn scale_target(opts: &Opts, a: &Analysis) -> Option<Scale> {
    if !opts.scale_1080 {
        return None;
    }
    let src = match a.crop.as_deref().and_then(parse_crop) {
        Some((w, h, _, _)) => (w, h),
        None => probe::video_size(&a.input_file)?,
    };
    fit_1080(src.0, src.1).map(|dst| Scale { src, dst })
}

/// Filtr CPU: skalowanie i/lub przeliczenie pełnego zakresu jasności (pc) na ograniczony (tv).
fn cpu_scale_range(scale: Option<&Scale>, range_fix: bool) -> Option<String> {
    let range = if range_fix { ":in_range=pc:out_range=tv" } else { "" };
    match scale {
        Some(s) => Some(format!("{}{range}", s.cpu())),
        None if range_fix => Some("scale=in_range=pc:out_range=tv".into()),
        None => None,
    }
}

/// "crop=W:H:X:Y" → (W, H, X, Y)
fn parse_crop(s: &str) -> Option<(u32, u32, u32, u32)> {
    let v: Vec<u32> = s.strip_prefix("crop=")?.split(':').map(|x| x.parse().ok()).collect::<Option<_>>()?;
    (v.len() == 4).then(|| (v[0], v[1], v[2], v[3]))
}

/// Dekoder QSV dla kodeka źródła (nazwa z ffprobe), jeśli ffmpeg go ma.
fn qsv_decoder(rt: &Runtime, src_codec: Option<&str>) -> Option<String> {
    let name = match src_codec? {
        "h264" => "h264_qsv",
        "hevc" => "hevc_qsv",
        "av1" => "av1_qsv",
        "vp9" => "vp9_qsv",
        "vp8" => "vp8_qsv",
        "mpeg2video" => "mpeg2_qsv",
        "vc1" => "vc1_qsv",
        "mjpeg" => "mjpeg_qsv",
        _ => return None,
    };
    rt.qsv_decoders.iter().any(|d| d == name).then(|| name.to_string())
}

/// Intel: QSV (dekodowanie QSV + vpp_qsv) albo VAAPI (dekodowanie VAAPI + filtry VAAPI).
/// Zwraca (argumenty, opis, dekoder).
fn build_args_intel(
    opts: &Opts,
    rt: &Runtime,
    a: &Analysis,
    src_codec: Option<&str>,
    out: &OutSpec,
    pf: &Path,
) -> (Vec<String>, String, String) {
    let c = &a.color;
    let dev = rt.intel_dev.as_ref().map(|d| d.to_string_lossy().to_string()).unwrap_or_default();
    // AV1 zawsze 10-bit (jak -highbitdepth w NVENC); HEVC 10-bit tylko dla źródeł HDR/10-bit.
    let ten_bit = (!c.is_sdr_8bit || opts.codec == Codec::Av1) && opts.codec != Codec::Avc;
    let crop = a.crop.as_deref().and_then(parse_crop);
    let scale = scale_target(opts, a);
    let denoise = opts.denoise && c.is_sdr_8bit && rt.denoise != Denoise::Off;
    // AVC ze źródła HDR: mapowanie tonów (sprzętowe Intel albo CPU — wtedy klatki przez pamięć).
    let tonemap = opts.codec == Codec::Avc && c.is_hdr();
    let range_fix = c.full_range() && !tonemap;
    let placebo = tonemap && rt.tonemap == Tonemap::Placebo && opts.engine == Engine::Qsv;
    let cpu_tonemap = tonemap && !placebo && !matches!(rt.tonemap, Tonemap::Qsv | Tonemap::Vaapi);
    let cq = opts.cq() as u32;

    let mut v = sv(&["-hide_banner", "-nostats", "-progress"]);
    v.push(pf.to_string_lossy().into());

    let mut notes: Vec<String> = vec![match (c.is_sdr_8bit, opts.codec) {
        (true, _) => "SDR 8-bit".into(),
        (false, Codec::Avc) if tonemap => format!(
            "HDR → SDR AVC 8-bit (tone mapping {})",
            if placebo { "libplacebo" } else if cpu_tonemap { "CPU" } else { "GPU Intel" }
        ),
        (false, Codec::Avc) => "SDR 10-bit → AVC 8-bit".into(),
        (false, _) => "HDR 10-bit".into(),
    }];
    if opts.codec == Codec::Av1 && c.is_sdr_8bit {
        notes.push("→ AV1 10-bit".into());
    }
    if let Some(s) = &scale {
        notes.push(format!("skalowanie {}x{} → {}x{}", s.src.0, s.src.1, s.dst.0, s.dst.1));
    }
    if range_fix {
        notes.push("pełny zakres → TV".into());
    }
    let decoder;
    let enc: Vec<String>;

    match opts.engine {
        Engine::Qsv => {
            // Windows, tryb "sysmem": bez urządzenia i pul ramek ffmpeg (patrz hw::WIN_QSV_DEVICES) —
            // dekoder QSV oddaje klatki do pamięci, filtry CPU, koder QSV sam wgrywa je na kartę.
            let sysmem = crate::hw::qsv_is_sysmem(&dev);
            v.extend(crate::hw::qsv_init_args(&dev));
            if placebo {
                // libplacebo bierze urządzenie Vulkan; koder QSV dostaje klatki z pamięci.
                v.extend(vulkan_args(&rt.vulkan_dev));
            } else if !sysmem {
                v.extend(sv(&["-filter_hw_device", "qs"]));
            }
            // Filtry na CPU (Windows "sysmem", mapowanie tonów na CPU albo libplacebo): klatki do pamięci.
            let cpu_filters = sysmem || cpu_tonemap || placebo;
            match qsv_decoder(rt, src_codec) {
                Some(d) if cpu_filters => {
                    v.extend(sv(&["-c:v", &d]));
                    decoder = format!("QSV ({d}, przez pamięć)");
                }
                Some(d) => {
                    v.extend(sv(&["-hwaccel", "qsv", "-hwaccel_device", "qs", "-hwaccel_output_format", "qsv"]));
                    v.extend(sv(&["-c:v", &d]));
                    decoder = format!("QSV ({d})");
                }
                None => decoder = format!("CPU (brak dekodera QSV dla {})", src_codec.unwrap_or("?")),
            }
            v.push("-i".into());
            v.push(a.input_file.to_string_lossy().into());

            if cpu_filters {
                let mut chain: Vec<String> = vec![];
                if let Some(cr) = &a.crop {
                    chain.push(cr.clone());
                }
                if placebo {
                    chain.push(tonemap_placebo(scale.as_ref(), if ten_bit { "p010le" } else { "nv12" }));
                } else if let Some(f) = cpu_scale_range(scale.as_ref(), range_fix) {
                    chain.push(f);
                }
                if cpu_tonemap {
                    chain.push(TONEMAP_CPU.into());
                }
                if denoise {
                    chain.push(DENOISE_CPU_FILTER.into());
                    notes.push("denoise CPU".into());
                }
                chain.push(format!("format={}", if ten_bit { "p010le" } else { "nv12" }));
                v.extend(sv(&["-vf", &chain.join(",")]));
            } else {
                // vpp_qsv przyjmuje klatki z karty i z pamięci (dekodowanie programowe) — jeden filtr na wszystko.
                let mut vpp: Vec<String> = vec![];
                if let Some((w, h, x, y)) = crop {
                    vpp.push(format!("cw={w}:ch={h}:cx={x}:cy={y}"));
                }
                if let Some(s) = &scale {
                    vpp.push(format!("w={}:h={}", s.dst.0, s.dst.1));
                }
                if denoise && rt.denoise == Denoise::Intel {
                    vpp.push(format!("denoise={DENOISE_QSV_LEVEL}"));
                    notes.push("denoise vpp_qsv".into());
                }
                if tonemap {
                    vpp.push(TONEMAP_QSV.into());
                }
                if range_fix {
                    vpp.push("out_range=tv".into());
                }
                vpp.push(format!("format={}", if ten_bit { "p010" } else { "nv12" }));
                v.extend(sv(&["-vf", &format!("vpp_qsv={}", vpp.join(":"))]));
            }

            enc = match opts.codec {
                Codec::Hevc => sv(&[
                    "-c:v", "hevc_qsv", "-preset", opts.qsv_preset, "-global_quality", &cq.to_string(),
                    "-profile:v", if ten_bit { "main10" } else { "main" },
                ]),
                Codec::Av1 => sv(&[
                    "-c:v", "av1_qsv", "-preset", opts.qsv_preset, "-global_quality", &cq.to_string(),
                    "-profile:v", "main",
                ]),
                Codec::Avc => sv(&[
                    "-c:v", "h264_qsv", "-preset", opts.qsv_preset, "-global_quality", &cq.to_string(),
                    "-profile:v", "high",
                ]),
            };
            notes.push(format!("ICQ {cq}"));
        }
        _ => {
            // VAAPI. Crop i odszumianie na CPU wymagają klatek w pamięci: dekodowanie nadal sprzętowe,
            // ale bez -hwaccel_output_format (ffmpeg sam pobiera klatki), potem hwupload.
            let sw_fmt = if ten_bit { "p010le" } else { "nv12" };
            let cpu_denoise = denoise && rt.denoise == Denoise::Cpu;
            let sysmem = crop.is_some() || cpu_denoise || cpu_tonemap;
            v.extend(sv(&["-init_hw_device", &format!("vaapi=va:{dev}"), "-filter_hw_device", "va"]));
            v.extend(sv(&["-hwaccel", "vaapi", "-hwaccel_device", "va"]));
            if !sysmem {
                v.extend(sv(&["-hwaccel_output_format", "vaapi"]));
            }
            decoder = "VAAPI (zapasowo CPU)".into();
            v.push("-i".into());
            v.push(a.input_file.to_string_lossy().into());

            let mut chain: Vec<String> = vec![];
            if sysmem {
                if let Some(cr) = &a.crop {
                    chain.push(cr.clone());
                }
                if let Some(f) = cpu_scale_range(scale.as_ref(), range_fix) {
                    chain.push(f);
                }
                if cpu_denoise {
                    chain.push(DENOISE_CPU_FILTER.into());
                    notes.push("denoise CPU".into());
                }
                if cpu_tonemap {
                    chain.push(TONEMAP_CPU.into());
                }
                // Po sprzętowym mapowaniu tonów klatki są jeszcze 10-bit — tonemap_vaapi zamieni je na nv12.
                let up_fmt = if tonemap && !cpu_tonemap { "p010le" } else { sw_fmt };
                chain.push(format!("format={up_fmt}"));
                chain.push("hwupload".into());
                if tonemap && !cpu_tonemap {
                    chain.push(TONEMAP_VAAPI.into());
                }
            } else {
                // Klatki z dekodera VAAPI przechodzą bez kopiowania; po dekodowaniu programowym — hwupload.
                chain.push("format=nv12|p010le|vaapi".into());
                chain.push("hwupload".into());
                let size = scale.as_ref().map(|s| format!("w={}:h={}:", s.dst.0, s.dst.1)).unwrap_or_default();
                if tonemap {
                    chain.push(TONEMAP_VAAPI.into());
                    if !size.is_empty() {
                        chain.push(format!("scale_vaapi={size}format=nv12"));
                    }
                } else {
                    let range = if range_fix { "out_range=tv:" } else { "" };
                    chain.push(format!("scale_vaapi={size}{range}format={}", if ten_bit { "p010" } else { "nv12" }));
                }
            }
            if denoise && rt.denoise == Denoise::Intel {
                chain.push(format!("denoise_vaapi=denoise={DENOISE_VAAPI_LEVEL}"));
                notes.push("denoise_vaapi".into());
            }
            v.extend(sv(&["-vf", &chain.join(",")]));

            enc = match opts.codec {
                Codec::Hevc => sv(&[
                    "-c:v", "hevc_vaapi", "-rc_mode", "CQP", "-qp", &cq.to_string(),
                    "-profile:v", if ten_bit { "main10" } else { "main" },
                ]),
                // AV1 VAAPI: base q index 0–255 — CQ przeliczone ×5 (25 → 125).
                Codec::Av1 => sv(&[
                    "-c:v", "av1_vaapi", "-rc_mode", "CQP", "-qp", &(cq * 5).min(255).to_string(),
                    "-profile:v", "main",
                ]),
                Codec::Avc => sv(&[
                    "-c:v", "h264_vaapi", "-rc_mode", "CQP", "-qp", &cq.to_string(), "-profile:v", "high",
                ]),
            };
            notes.push(format!("CQP {cq}"));
        }
    }

    v.extend(out.streams.iter().cloned());
    v.extend(scope_v(enc));
    let color = if c.is_sdr_8bit || opts.codec == Codec::Avc {
        sdr_tags(c, tonemap)
    } else {
        sv(&["-color_primaries", "bt2020", "-color_trc", "smpte2084", "-colorspace", "bt2020nc"])
    };
    v.extend(scope_v(color));
    v.extend(out.tail.iter().cloned());
    (v, notes.join(" · "), decoder)
}

/// Sukces = niepusty plik + progress=end + właściwy kodek.
/// Sprawdzenie pliku po kodowaniu; Err = powód odrzucenia (do logu i panelu).
fn result_check(opts: &Opts, out: &Path, pf: &Path) -> Result<(), String> {
    match fs::metadata(out) {
        Ok(m) if m.len() > 0 => {}
        Ok(_) => return Err(format!("pusty plik {}", out.display())),
        Err(e) => return Err(format!("brak pliku {}: {e}", out.display())),
    }
    if !read_progress(pf).ended {
        return Err("zapis pliku nie został zakończony".into());
    }
    let (codec, err) = probe::video_codec_diag(out);
    match codec.as_deref() {
        Some(c) if c == opts.codec.name() => Ok(()),
        Some(c) => Err(format!("kodek wideo wyniku {c}, oczekiwany {}", opts.codec.name())),
        None => Err(format!("nie udało się odczytać wideo z pliku{}", if err.is_empty() { String::new() } else { format!(": {err}") })),
    }
}

fn start(opts: &Opts, rt: &Runtime, a: &Analysis, slot: usize, panel: &Panel) -> std::io::Result<Running> {
    let p = opts.paths_for(&a.input_file);
    let key = sanitize(&a.filename);
    let pf = rt.progress_dir.join(format!("{key}.progress"));
    let _ = fs::remove_file(&pf);
    File::create(&pf)?;

    let dur_us = probe::container_duration(&a.input_file).map(|d| d * 1e6);
    panel.set_progress(&a.filename, pf.clone(), dur_us);

    if opts.codec == Codec::Avc && a.color.is_hdr() && rt.tonemap == Tonemap::None {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "źródło HDR, a biblioteki FFmpeg nie mają filtrów mapowania tonów (zscale/tonemap) — AVC niemożliwe; wybierz HEVC albo AV1",
        ));
    }
    // DV/HDR10+ w locie: RPU i SEI HDR10+ doklejane do pakietów kodera przed zapisem do kontenera.
    let live = !opts.direct_encode(a.has_dovi, a.has_hdr10plus);
    let part = PathBuf::from(format!("{}.part", p.output.display()));
    let _ = fs::remove_file(&part);
    let mut out = direct_out(opts, a, &p, &part);
    if live && p.container == Container::Mp4 {
        // Rekord konfiguracji Dolby Vision (dvvC) w MP4 wymaga zgody na elementy spoza normy.
        out.tail.splice(0..0, sv(&["-strict", "unofficial"]));
    }
    let (args, note, decoder) = if opts.engine == Engine::Nvenc {
        let (args, note) = build_args_nvenc(opts, rt, a, &out, &pf);
        (args, note, "CUDA/NVDEC".to_string())
    } else {
        let src = probe::video_codec(&a.input_file);
        build_args_intel(opts, rt, a, src.as_deref(), &out, &pf)
    };
    let eng = opts.engine.label();
    let mode = if live {
        format!(" — DV/HDR10+ w locie do {}", p.container.ext().to_uppercase())
    } else {
        format!(" — od razu do {}", p.container.ext().to_uppercase())
    };
    panel.set(&a.filename, State::Nvenc, format!("{eng} {slot}/2 — {note} — dekoder: {decoder}{mode}"));

    let job = crate::av::Job::parse(&args).map_err(|e| std::io::Error::other(format!("{e:#}")))?;
    crate::log::file_line(&format!("[kodowanie] {} — zadanie: {}", a.filename, args.join(" ")));
    let rpu = (live && a.has_dovi && !opts.no_dv).then(|| p.temp_rpu.clone());
    let json = (live && a.has_hdr10plus && !opts.no_hdr10plus).then(|| p.temp_hdr10plus.clone());
    let l5 = scale_target(opts, a).map(|s| s.factors());
    let handle = thread::Builder::new().name(format!("kodowanie-{slot}")).spawn(move || {
        struct Active;
        impl Drop for Active {
            fn drop(&mut self) {
                proc::encoding_end();
            }
        }
        proc::encoding_begin();
        let _active = Active;
        let inject = if live {
            Some(crate::av::InjectCfg { meta: crate::inject::Metadata::load(rpu.as_deref(), json.as_deref(), l5)? })
        } else {
            None
        };
        crate::av::run(&job, crate::av::Hooks { inject, stop: None })
    })?;
    Ok(Running { job: handle, name: a.filename.clone(), live, output: part, final_out: p.output.clone(), pf })
}

fn finish(opts: &Opts, r: Running, panel: &Panel) {
    let res = r.job.join().unwrap_or_else(|_| Err(anyhow::anyhow!("wątek kodowania zakończył się awarią")));
    if proc::cancelled() {
        // Częściowy plik nie może zostać uznany za ukończony.
        let _ = fs::remove_file(&r.output);
        panel.set(&r.name, State::Waiting, "Przerwano");
        return;
    }
    let check = match &res {
        Ok(rep) => {
            if crate::log::full() {
                crate::log::file_line(&format!("[kodowanie] {} — {} klatek; komunikaty bibliotek:\n{}", r.name, rep.frames, rep.log.join("\n")));
            }
            if let Some(st) = &rep.inject {
                let dv = if st.rpus > 0 { format!(", Dolby Vision poziom {}", st.dv_level) } else { String::new() };
                let h10 = if st.hdr10plus > 0 { ", HDR10+" } else { "" };
                plog!(" -> [{}] metadane w locie: {} klatek {}x{}{dv}{h10}", r.name, st.frames, st.width, st.height);
                if st.converted_p7 {
                    plog!("    RPU profilu 7 przerobione na 8.1 (kodowana jest tylko warstwa bazowa)");
                }
                for w in st.warnings() {
                    plog!("    UWAGA: {w}");
                }
            }
            result_check(opts, &r.output, &r.pf)
        }
        Err(_) => Err(String::new()),
    };
    if check.is_ok() {
        // Gotowy plik wynikowy dopiero po pełnym zapisie (.part → nazwa docelowa).
        let fin = &r.final_out;
        if let Err(e) = fs::rename(&r.output, fin) {
            panel.set(&r.name, State::Error, format!("nie można zapisać {}: {e}", fin.display()));
            plog!(" -> [{}]: BŁĄD zapisu pliku wynikowego: {e}", r.name);
            return;
        }
        let note = if r.live { "Zakodowano z DV/HDR10+ w locie do pliku wynikowego" } else { "Zakodowano od razu do pliku wynikowego" };
        panel.set(&r.name, State::NvencDone, note);
        plog!(" -> [{}]: ZAKOŃCZONY + PLIK OK", r.name);
        return;
    }
    match (&res, check.err().filter(|w| !w.is_empty())) {
        (Ok(_), Some(why)) => {
            panel.set(&r.name, State::Error, format!("{} — plik odrzucony: {why}", opts.engine.label()));
            plog!(" -> [{}]: kodowanie zakończone, ale plik odrzucony: {why}", r.name);
            // Kodowanie skończyło się bez błędu — plik zostaje do sprawdzenia (nie jest uznawany za gotowy).
            let keep = PathBuf::from(format!("{}.odrzucony", r.output.display()));
            if fs::rename(&r.output, &keep).is_ok() {
                plog!("    plik zostawiony do sprawdzenia: {}", keep.display());
            }
        }
        (res, _) => {
            let msg = res.as_ref().err().map(|e| format!("{e:#}")).unwrap_or_default();
            let first = msg.lines().next().unwrap_or("błąd kodowania").to_string();
            panel.set(&r.name, State::Error, format!("{}: {first}", opts.engine.label()));
            plog!(" -> [{}]: BŁĄD kodowania:", r.name);
            for l in msg.lines() {
                plog!("    {l}");
            }
            let _ = fs::remove_file(&r.output);
        }
    }
}

fn reap(opts: &Opts, running: &mut Vec<Running>, panel: &Panel) {
    let mut i = 0;
    while i < running.len() {
        if running[i].job.is_finished() {
            let r = running.remove(i);
            finish(opts, r, panel);
        } else {
            i += 1;
        }
    }
}

/// Przerwanie: wątki kodowania same kończą pracę (sprawdzają znacznik przerwania); kasujemy niedokończone pliki.
fn abort_all(running: &mut Vec<Running>, panel: &Panel) {
    for r in running.drain(..) {
        let _ = r.job.join();
        let _ = fs::remove_file(&r.output);
        panel.set(&r.name, State::Waiting, "Przerwano");
    }
}

/// Dwa równoległe sloty kodowania; dekodowanie sprzętowe tej samej karty (NVDEC / QSV / VAAPI).
pub fn run_stage(opts: &Opts, rt: &Runtime, files: &[Analysis], panel: &Panel) {
    panel.draw(&format!("ETAP 2/3 — DUAL {}", opts.engine.label()));
    let mut running: Vec<Running> = vec![];

    for a in files {
        if proc::cancelled() {
            abort_all(&mut running, panel);
            return;
        }
        let p = opts.paths_for(&a.input_file);
        if p.output.exists() {
            panel.set(&a.filename, State::Done, "Plik wynikowy już istnieje");
            continue;
        }
        while running.len() >= 2 {
            reap(opts, &mut running, panel);
            if proc::cancelled() {
                abort_all(&mut running, panel);
                return;
            }
            if running.len() >= 2 {
                thread::sleep(Duration::from_millis(500));
            }
        }

        let slot = running.len() + 1;
        match start(opts, rt, a, slot, panel) {
            Ok(r) => {
                plog!(" -> [{}] start (koder {slot}/2)", a.filename);
                running.push(r);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
                plog!(" -> [{}] POMINIĘTO: {e}", a.filename);
                panel.set(&a.filename, State::Error, e.to_string());
            }
            Err(e) => {
                panel.set(&a.filename, State::Error, format!("nie można rozpocząć kodowania: {e}"));
            }
        }
        thread::sleep(Duration::from_millis(300));
    }

    while !running.is_empty() {
        reap(opts, &mut running, panel);
        if proc::cancelled() {
            abort_all(&mut running, panel);
            return;
        }
        if !running.is_empty() {
            thread::sleep(Duration::from_millis(500));
        }
    }
}

#[cfg(test)]
mod scale_tests {
    use super::fit_1080;

    #[test]
    fn fits_into_1080p() {
        assert_eq!(fit_1080(3840, 2160), Some((1920, 1080)));
        assert_eq!(fit_1080(3840, 1600), Some((1920, 800)));
        assert_eq!(fit_1080(3832, 1600), Some((1920, 802)));
        assert_eq!(fit_1080(2880, 2160), Some((1440, 1080)));
        assert_eq!(fit_1080(4096, 1716), Some((1920, 804)));
        assert_eq!(fit_1080(1920, 1080), None);
        assert_eq!(fit_1080(1280, 720), None);
    }
}
