//! Wykonanie zadania: plik (albo graf źródłowy) → dekodery → filtry → kodery → kontener.
//! Jedno zadanie działa w jednym wątku; biblioteki same używają wątków w dekoderach i filtrach.
use super::{chk, cs, cstr, dict_entries, ff, has_opt, Dict, Job, Map};
use crate::inject::{PacketInjector, Stats};
use anyhow::{anyhow, bail, Context, Result};
use std::{
    collections::VecDeque,
    ffi::{c_int, c_void},
    io::Write,
    ptr::{null, null_mut},
    sync::{atomic::{AtomicBool, Ordering}, Arc},
    time::Instant,
};

const NOPTS: i64 = ff::AV_NOPTS_VALUE;
const TBQ: ff::AVRational = ff::AVRational { num: 1, den: ff::AV_TIME_BASE };
const EAGAIN: c_int = -(ff::EAGAIN);
const EOF: c_int = ff::AVERROR_EOF;
/// AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX
const HW_DEVICE_CTX: c_int = 1;
/// AV_PIX_FMT_FLAG_HWACCEL
const PIX_HWACCEL: u64 = 1 << 3;

/// AVDOVIDecoderConfigurationRecord (libavutil/dovi_meta.h — poza gotowymi wiązaniami).
#[repr(C)]
struct DoviRecord {
    dv_version_major: u8,
    dv_version_minor: u8,
    dv_profile: u8,
    dv_level: u8,
    rpu_present_flag: u8,
    el_present_flag: u8,
    bl_present_flag: u8,
    dv_bl_signal_compatibility_id: u8,
    dv_md_compression: u8,
}

extern "C" {
    fn av_dovi_alloc(size: *mut usize) -> *mut DoviRecord;
}

/// Dolby Vision / HDR10+ doklejane do pakietów kodera przed zapisem do kontenera.
pub struct InjectCfg {
    pub meta: crate::inject::Metadata,
}

#[derive(Default)]
pub struct Hooks {
    pub inject: Option<InjectCfg>,
    /// Zewnętrzne zatrzymanie (np. limit czasu testu); przerwanie przez użytkownika działa zawsze.
    pub stop: Option<Arc<AtomicBool>>,
}

#[derive(Default, Debug)]
pub struct Report {
    /// Zakodowane klatki wideo.
    pub frames: u64,
    pub inject: Option<Stats>,
    /// Komunikaty bibliotek z przebiegu zadania.
    pub log: Vec<String>,
    /// Ostatnie metadane klatek z filtrów (np. lavfi.cropdetect.*).
    pub frame_meta: Vec<(String, String)>,
}

fn is_again(r: c_int) -> bool {
    r == EAGAIN || r == EOF
}

fn q2d(q: ff::AVRational) -> f64 {
    if q.den == 0 { 0.0 } else { q.num as f64 / q.den as f64 }
}

// ------------------------------------------------------------------------------------------- urządzenia

struct Devices {
    list: Vec<(String, ff::AVHWDeviceType, *mut ff::AVBufferRef)>,
}

impl Devices {
    fn by_name(&self, name: &str) -> Option<*mut ff::AVBufferRef> {
        self.list.iter().find(|d| d.0 == name).map(|d| d.2)
    }
    fn by_type(&self, t: ff::AVHWDeviceType) -> Option<*mut ff::AVBufferRef> {
        self.list.iter().find(|d| d.1 as i32 == t as i32).map(|d| d.2)
    }

    /// `typ[=nazwa][:urządzenie][,klucz=wartość…]` albo `typ[=nazwa]@źródło` (urządzenie pochodne).
    unsafe fn init(&mut self, spec: &str) -> Result<()> {
        let cut = spec.find([':', '@', ',']).unwrap_or(spec.len());
        let (head, rest) = spec.split_at(cut);
        let (tname, name) = head.split_once('=').unwrap_or((head, head));
        let t = ff::av_hwdevice_find_type_by_name(cs(tname).as_ptr());
        if t as i32 == ff::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE as i32 {
            bail!("nieznany rodzaj urządzenia sprzętowego „{tname}”");
        }
        let mut dev: *mut ff::AVBufferRef = null_mut();
        if let Some(src) = rest.strip_prefix('@') {
            let parent = self.by_name(src).ok_or_else(|| anyhow!("urządzenie „{src}” nie istnieje"))?;
            chk(ff::av_hwdevice_ctx_create_derived(&mut dev, t, parent, 0), &format!("urządzenie {spec}"))?;
        } else {
            let rest = rest.strip_prefix(':').unwrap_or(rest);
            let (device, optstr) = match rest.strip_prefix(',') {
                Some(o) => ("", o),
                None => rest.split_once(',').unwrap_or((rest, "")),
            };
            let mut opts = Dict::new();
            for kv in optstr.split(',').filter(|s| !s.is_empty()) {
                let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                opts.set(k, v);
            }
            let d = cs(device);
            let dp = if device.is_empty() { null() } else { d.as_ptr() };
            chk(ff::av_hwdevice_ctx_create(&mut dev, t, dp, opts.0, 0), &format!("urządzenie {spec}"))?;
        }
        self.list.push((name.to_string(), t, dev));
        Ok(())
    }
}

impl Drop for Devices {
    fn drop(&mut self) {
        for d in &mut self.list {
            unsafe { ff::av_buffer_unref(&mut d.2) }
        }
    }
}

// ------------------------------------------------------------------------------------------- ścieżki

struct Video {
    dec: *mut ff::AVCodecContext,
    /// Rodzaj urządzenia dekodera (dla get_format) — własność ścieżki.
    dec_hw: *mut i32,
    keep_hw: bool,
    graph: *mut ff::AVFilterGraph,
    src: *mut ff::AVFilterContext,
    sink: *mut ff::AVFilterContext,
    enc: *mut ff::AVCodecContext,
    codec: *const ff::AVCodec,
    in_tb: ff::AVRational,
    in_fr: ff::AVRational,
    last_pts: i64,
    /// Ostatni czas klatki oddanej koderowi (w podstawie czasu kodera).
    out_last: i64,
    sent: u64,
    /// Właściwości pakietów czekających we wstrzykiwaczu DV/HDR10+ (kolejność dekodowania).
    props: VecDeque<*mut ff::AVPacket>,
}

struct Audio {
    dec: *mut ff::AVCodecContext,
    graph: *mut ff::AVFilterGraph,
    src: *mut ff::AVFilterContext,
    sink: *mut ff::AVFilterContext,
    enc: *mut ff::AVCodecContext,
    codec: *const ff::AVCodec,
    next_pts: i64,
}

struct Subs {
    dec: *mut ff::AVCodecContext,
    enc: *mut ff::AVCodecContext,
}

enum Path {
    Copy,
    Video(Video),
    Audio(Audio),
    Subs(Subs),
}

struct Out {
    ist: usize,
    st: *mut ff::AVStream,
    kind: u8,
    kidx: usize,
    path: Path,
    ready: bool,
}

struct Engine<'a> {
    job: &'a Job,
    ictx: *mut ff::AVFormatContext,
    octx: *mut ff::AVFormatContext,
    /// Graf źródłowy (lavfi) zamiast pliku.
    lavfi: *mut ff::AVFilterGraph,
    lavfi_sink: *mut ff::AVFilterContext,
    devices: Devices,
    outs: Vec<Out>,
    header: bool,
    /// Pakiety sprzed zapisu nagłówka: (strumień wyjściowy, pakiet, jego podstawa czasu).
    queue: Vec<(usize, *mut ff::AVPacket, ff::AVRational)>,
    inject: Option<PacketInjector>,
    frame: *mut ff::AVFrame,
    filt: *mut ff::AVFrame,
    pkt: *mut ff::AVPacket,
    opkt: *mut ff::AVPacket,
    frames: u64,
    done: bool,
    out_us: i64,
    started: Instant,
    last_progress: Instant,
    frame_meta: Vec<(String, String)>,
    stop: Option<Arc<AtomicBool>>,
}

unsafe extern "C" fn get_format(s: *mut ff::AVCodecContext, fmts: *const ff::AVPixelFormat) -> ff::AVPixelFormat {
    let want = if (*s).opaque.is_null() { -1 } else { *((*s).opaque as *const i32) };
    let mut p = fmts as *const i32;
    let mut sw = -1;
    while *p != -1 {
        let f = *p;
        let desc = ff::av_pix_fmt_desc_get(std::mem::transmute::<i32, ff::AVPixelFormat>(f));
        if desc.is_null() || (*desc).flags & PIX_HWACCEL == 0 {
            if sw < 0 {
                sw = f;
            }
        } else {
            let mut i = 0;
            loop {
                let cfg = ff::avcodec_get_hw_config((*s).codec, i);
                if cfg.is_null() {
                    break;
                }
                if (*cfg).pix_fmt as i32 == f && (*cfg).methods & HW_DEVICE_CTX != 0 && (*cfg).device_type as i32 == want {
                    return std::mem::transmute::<i32, ff::AVPixelFormat>(f);
                }
                i += 1;
            }
        }
        p = p.add(1);
    }
    std::mem::transmute::<i32, ff::AVPixelFormat>(sw)
}

unsafe fn supported<T: Copy>(codec: *const ff::AVCodec, what: ff::AVCodecConfig) -> Vec<T> {
    let mut p: *const c_void = null();
    let mut n: c_int = 0;
    if ff::avcodec_get_supported_config(null(), codec, what, 0, &mut p, &mut n) < 0 || p.is_null() || n <= 0 {
        return vec![];
    }
    std::slice::from_raw_parts(p as *const T, n as usize).to_vec()
}

unsafe fn layout_name(l: *const ff::AVChannelLayout) -> String {
    let mut buf = [0 as std::ffi::c_char; 128];
    ff::av_channel_layout_describe(l, buf.as_mut_ptr(), buf.len());
    cstr(buf.as_ptr())
}

/// Graf z opisu tekstowego między gotowym źródłem a nowym odbiornikiem; filtry sprzętowe dostają urządzenie.
unsafe fn build_graph(
    graph: *mut ff::AVFilterGraph,
    src: *mut ff::AVFilterContext,
    sink_name: &str,
    desc: &str,
    device: Option<*mut ff::AVBufferRef>,
) -> Result<*mut ff::AVFilterContext> {
    let mut sink: *mut ff::AVFilterContext = null_mut();
    let f = ff::avfilter_get_by_name(cs(sink_name).as_ptr());
    chk(ff::avfilter_graph_create_filter(&mut sink, f, cs("out").as_ptr(), null(), null_mut(), graph), "odbiornik grafu filtrów")?;
    let mut inputs = ff::avfilter_inout_alloc();
    (*inputs).name = ff::av_strdup(cs("out").as_ptr());
    (*inputs).filter_ctx = sink;
    (*inputs).pad_idx = 0;
    (*inputs).next = null_mut();
    let mut outputs: *mut ff::AVFilterInOut = null_mut();
    if !src.is_null() {
        outputs = ff::avfilter_inout_alloc();
        (*outputs).name = ff::av_strdup(cs("in").as_ptr());
        (*outputs).filter_ctx = src;
        (*outputs).pad_idx = 0;
        (*outputs).next = null_mut();
    }
    let r = ff::avfilter_graph_parse_ptr(graph, cs(desc).as_ptr(), &mut inputs, &mut outputs, null_mut());
    ff::avfilter_inout_free(&mut inputs);
    ff::avfilter_inout_free(&mut outputs);
    chk(r, &format!("filtry „{desc}”"))?;
    if let Some(dev) = device {
        for i in 0..(*graph).nb_filters as usize {
            let fc = *(*graph).filters.add(i);
            // AVFILTER_FLAG_HWDEVICE
            if (*(*fc).filter).flags & (1 << 4) != 0 && (*fc).hw_device_ctx.is_null() {
                (*fc).hw_device_ctx = ff::av_buffer_ref(dev);
            }
        }
    }
    chk(ff::avfilter_graph_config(graph, null_mut()), &format!("konfiguracja filtrów „{desc}”"))?;
    Ok(sink)
}

impl Engine<'_> {
    fn stopped(&self) -> bool {
        crate::proc::cancelled() || self.stop.as_ref().is_some_and(|s| s.load(Ordering::Relaxed))
    }

    unsafe fn ist(&self, i: usize) -> *mut ff::AVStream {
        *(*self.ictx).streams.add(i)
    }

    // ---------------------------------------------------------------------------------- przygotowanie

    unsafe fn open_input(&mut self) -> Result<()> {
        let job = self.job;
        if job.lavfi {
            self.lavfi = ff::avfilter_graph_alloc();
            self.lavfi_sink = build_graph(self.lavfi, null_mut(), "buffersink", &job.input, None)?;
            return Ok(());
        }
        chk(
            ff::avformat_open_input(&mut self.ictx, cs(&job.input).as_ptr(), null(), null_mut()),
            &format!("otwarcie {}", job.input),
        )?;
        chk(ff::avformat_find_stream_info(self.ictx, null_mut()), "odczyt strumieni")?;
        if let Some(ss) = job.ss.filter(|s| *s > 0.0) {
            let ts = (ss * ff::AV_TIME_BASE as f64) as i64 + self.start_time();
            // Najbliższa klatka kluczowa przed wskazanym miejscem (próbki do analizy, nie cięcie co do klatki).
            ff::avformat_seek_file(self.ictx, -1, i64::MIN, ts, ts, 0);
        }
        Ok(())
    }

    /// Dekoder strumienia wejściowego; dla wideo także dekodowanie sprzętowe według zadania.
    unsafe fn open_decoder(&mut self, ist: usize, video: bool) -> Result<(*mut ff::AVCodecContext, *mut i32, bool)> {
        let st = self.ist(ist);
        let par = (*st).codecpar;
        let job = self.job;
        let codec = match job.vdecoder.as_deref().filter(|_| video) {
            Some(name) => {
                let c = ff::avcodec_find_decoder_by_name(cs(name).as_ptr());
                if c.is_null() {
                    bail!("brak dekodera {name}");
                }
                c
            }
            None => {
                let c = ff::avcodec_find_decoder((*par).codec_id);
                if c.is_null() {
                    bail!("brak dekodera dla {}", cstr(ff::avcodec_get_name((*par).codec_id)));
                }
                c
            }
        };
        let dec = ff::avcodec_alloc_context3(codec);
        chk(ff::avcodec_parameters_to_context(dec, par), "parametry dekodera")?;
        (*dec).pkt_timebase = (*st).time_base;
        let mut hw: *mut i32 = null_mut();
        let mut keep_hw = false;
        if video {
            (*dec).framerate = ff::av_guess_frame_rate(self.ictx, st, null_mut());
            if let Some(accel) = &job.hwaccel {
                let t = ff::av_hwdevice_find_type_by_name(cs(accel).as_ptr());
                let dev = match job.hwaccel_device.as_deref().and_then(|n| self.devices.by_name(n)) {
                    Some(d) => Some(d),
                    None => match self.devices.by_type(t) {
                        Some(d) => Some(d),
                        None => {
                            let spec = match &job.hwaccel_device {
                                Some(d) => format!("{accel}=_dec:{d}"),
                                None => format!("{accel}=_dec"),
                            };
                            match self.devices.init(&spec) {
                                Ok(()) => self.devices.by_name("_dec"),
                                Err(e) => {
                                    // Jak ffmpeg: bez urządzenia dekodowanie programowe.
                                    ff::av_log(null_mut(), 24, cs("%s\n").as_ptr(), cs(&format!("dekodowanie sprzętowe niedostępne ({e:#}) — dekoder programowy")).as_ptr());
                                    None
                                }
                            }
                        }
                    },
                };
                if let Some(dev) = dev {
                    (*dec).hw_device_ctx = ff::av_buffer_ref(dev);
                    hw = Box::into_raw(Box::new(t as i32));
                    (*dec).opaque = hw as *mut c_void;
                    (*dec).get_format = Some(get_format);
                    keep_hw = job.hwaccel_output.is_some();
                }
            }
        }
        let mut opts = Dict::new();
        opts.set("threads", "auto");
        chk(ff::avcodec_open2(dec, codec, &mut opts.0), &format!("dekoder {}", cstr((*codec).name)))?;
        Ok((dec, hw, keep_hw))
    }

    unsafe fn setup_outputs(&mut self) -> Result<()> {
        let job = self.job;
        let fmt = job.format.as_ref().map(|f| cs(f));
        let null_out = job.output == "-";
        let name = cs(&job.output);
        chk(
            ff::avformat_alloc_output_context2(
                &mut self.octx,
                null(),
                fmt.as_ref().map_or(null(), |f| f.as_ptr()),
                if null_out { null() } else { name.as_ptr() },
            ),
            "kontener wyjściowy",
        )?;

        // Strumienie wejściowe do przeniesienia.
        let mut picks: Vec<usize> = vec![];
        if job.lavfi {
            picks.push(0);
        } else {
            let n = (*self.ictx).nb_streams as usize;
            let kind_of = |i: usize| match (*(*self.ist(i)).codecpar).codec_type {
                ff::AVMediaType::AVMEDIA_TYPE_VIDEO => b'v',
                ff::AVMediaType::AVMEDIA_TYPE_AUDIO => b'a',
                ff::AVMediaType::AVMEDIA_TYPE_SUBTITLE => b's',
                _ => 0,
            };
            if job.maps.is_empty() {
                picks.extend((0..n).find(|&i| kind_of(i) == b'v'));
            }
            for m in &job.maps {
                let idx = match *m {
                    Map::Index(i) if i < n => i,
                    Map::Index(i) => bail!("brak strumienia {i} w pliku wejściowym"),
                    Map::KindIdx(k, nth) => (0..n)
                        .filter(|&i| kind_of(i) == k)
                        .nth(nth)
                        .ok_or_else(|| anyhow!("brak strumienia {}:{nth} w pliku wejściowym", k as char))?,
                };
                let k = kind_of(idx);
                if k == 0 || (k == b'a' && job.no_audio) || (k == b's' && job.no_subs) {
                    continue;
                }
                picks.push(idx);
            }
        }
        if picks.is_empty() {
            bail!("brak strumieni do zapisania");
        }

        let mut counts = [0usize; 3];
        for ist in picks {
            let kind = if job.lavfi {
                b'v'
            } else {
                match (*(*self.ist(ist)).codecpar).codec_type {
                    ff::AVMediaType::AVMEDIA_TYPE_VIDEO => b'v',
                    ff::AVMediaType::AVMEDIA_TYPE_AUDIO => b'a',
                    _ => b's',
                }
            };
            let slot = match kind {
                b'v' => 0,
                b'a' => 1,
                _ => 2,
            };
            let kidx = counts[slot];
            counts[slot] += 1;
            let st = ff::avformat_new_stream(self.octx, null());
            if st.is_null() {
                bail!("nie można utworzyć strumienia wyjściowego");
            }
            // Bez zapisu (format „null”) wideo nie wymaga kodera — klatki przechodzą bez zmian.
            let null_video = kind == b'v' && job.format.as_deref() == Some("null");
            let codec = job.codec_for(kind, kidx).unwrap_or(if null_video { "wrapped_avframe" } else { "copy" });
            let mut ready = false;
            let path = if codec == "copy" {
                if job.lavfi {
                    bail!("źródło testowe wymaga kodera");
                }
                let src = self.ist(ist);
                chk(ff::avcodec_parameters_copy((*st).codecpar, (*src).codecpar), "parametry strumienia")?;
                (*(*st).codecpar).codec_tag = 0;
                (*st).time_base = (*src).time_base;
                (*st).avg_frame_rate = (*src).avg_frame_rate;
                (*st).r_frame_rate = (*src).r_frame_rate;
                (*st).sample_aspect_ratio = (*src).sample_aspect_ratio;
                ready = true;
                Path::Copy
            } else {
                let enc = ff::avcodec_find_encoder_by_name(cs(codec).as_ptr());
                if enc.is_null() {
                    bail!("biblioteki FFmpeg nie mają kodera {codec}");
                }
                match kind {
                    b'v' => {
                        let (dec, dec_hw, keep_hw) =
                            if job.lavfi { (null_mut(), null_mut(), false) } else { self.open_decoder(ist, true)? };
                        let (in_tb, in_fr) = if job.lavfi {
                            (ff::av_buffersink_get_time_base(self.lavfi_sink), ff::av_buffersink_get_frame_rate(self.lavfi_sink))
                        } else {
                            ((*self.ist(ist)).time_base, (*dec).framerate)
                        };
                        Path::Video(Video {
                            dec,
                            dec_hw,
                            keep_hw,
                            graph: null_mut(),
                            src: null_mut(),
                            sink: null_mut(),
                            enc: null_mut(),
                            codec: enc,
                            in_tb,
                            in_fr,
                            last_pts: NOPTS,
                            out_last: NOPTS,
                            sent: 0,
                            props: VecDeque::new(),
                        })
                    }
                    b'a' => {
                        let (dec, _, _) = self.open_decoder(ist, false)?;
                        Path::Audio(Audio { dec, graph: null_mut(), src: null_mut(), sink: null_mut(), enc: null_mut(), codec: enc, next_pts: NOPTS })
                    }
                    _ => {
                        let (dec, _, _) = self.open_decoder(ist, false)?;
                        let e = ff::avcodec_alloc_context3(enc);
                        (*e).time_base = TBQ;
                        if !(*dec).subtitle_header.is_null() && (*dec).subtitle_header_size > 0 {
                            let n = (*dec).subtitle_header_size as usize;
                            (*e).subtitle_header = ff::av_mallocz(n + 1) as *mut u8;
                            std::ptr::copy_nonoverlapping((*dec).subtitle_header, (*e).subtitle_header, n);
                            (*e).subtitle_header_size = n as c_int;
                        }
                        if (*(*self.octx).oformat).flags & ff::AVFMT_GLOBALHEADER != 0 {
                            (*e).flags |= ff::AV_CODEC_FLAG_GLOBAL_HEADER as c_int;
                        }
                        chk(ff::avcodec_open2(e, enc, null_mut()), &format!("koder napisów {codec}"))?;
                        chk(ff::avcodec_parameters_from_context((*st).codecpar, e), "parametry napisów")?;
                        (*st).time_base = TBQ;
                        ready = true;
                        Path::Subs(Subs { dec, enc: e })
                    }
                }
            };
            if !job.lavfi {
                let src = self.ist(ist);
                (*st).disposition = (*src).disposition;
                ff::av_dict_copy(&mut (*st).metadata, (*src).metadata, 0);
                if !matches!(path, Path::Copy) {
                    ff::av_dict_set(&mut (*st).metadata, cs("encoder").as_ptr(), null(), 0);
                }
            }
            self.outs.push(Out { ist, st, kind, kidx, path, ready });
        }

        // Metadane pliku, rozdziały, znaczniki ustawione w zadaniu.
        if !job.lavfi {
            if job.map_metadata.unwrap_or(0) >= 0 {
                ff::av_dict_copy(&mut (*self.octx).metadata, (*self.ictx).metadata, 0);
            }
            if job.map_chapters.unwrap_or(0) >= 0 {
                self.copy_chapters();
            }
        }
        for (target, k, v) in &job.metadata {
            let (key, val) = (cs(k), cs(v));
            let vp = if v.is_empty() { null() } else { val.as_ptr() };
            if target.is_empty() {
                ff::av_dict_set(&mut (*self.octx).metadata, key.as_ptr(), vp, 0);
                continue;
            }
            let spec = match target.strip_prefix('s').map(|r| r.trim_start_matches(':')) {
                Some(r) => super::Spec::parse(r)?,
                None => bail!("nieobsługiwany cel metadanych „{target}”"),
            };
            for o in self.outs.iter().filter(|o| spec.matches(o.kind, o.kidx)) {
                ff::av_dict_set(&mut (*o.st).metadata, key.as_ptr(), vp, 0);
            }
        }

        if (*(*self.octx).oformat).flags & ff::AVFMT_NOFILE == 0 {
            chk(
                ff::avio_open(&mut (*self.octx).pb, name.as_ptr(), ff::AVIO_FLAG_WRITE),
                &format!("zapis {}", job.output),
            )?;
        }
        Ok(())
    }

    unsafe fn copy_chapters(&mut self) {
        let n = (*self.ictx).nb_chapters as usize;
        if n == 0 {
            return;
        }
        let start = self.start_time();
        let arr = ff::av_calloc(n, std::mem::size_of::<*mut ff::AVChapter>()) as *mut *mut ff::AVChapter;
        if arr.is_null() {
            return;
        }
        for i in 0..n {
            let src = *(*self.ictx).chapters.add(i);
            let c = ff::av_mallocz(std::mem::size_of::<ff::AVChapter>()) as *mut ff::AVChapter;
            let off = ff::av_rescale_q(start, TBQ, (*src).time_base);
            (*c).id = (*src).id;
            (*c).time_base = (*src).time_base;
            (*c).start = ((*src).start - off).max(0);
            (*c).end = ((*src).end - off).max(0);
            ff::av_dict_copy(&mut (*c).metadata, (*src).metadata, 0);
            *arr.add(i) = c;
        }
        (*self.octx).chapters = arr;
        (*self.octx).nb_chapters = n as u32;
    }

    unsafe fn start_time(&self) -> i64 {
        if self.ictx.is_null() || (*self.ictx).start_time == NOPTS { 0 } else { (*self.ictx).start_time }
    }

    // ---------------------------------------------------------------------------------- zapis

    /// Opcje kontenera: te z zadania bez wskazania strumienia, które zna muxer.
    unsafe fn mux_opts(&self) -> Dict {
        let mut d = Dict::new();
        let of = (*self.octx).oformat;
        for (n, s, v) in &self.job.opts {
            if *s == super::Spec::All && (has_opt(ff::avformat_get_class(), n, None) || has_opt((*of).priv_class, n, None)) {
                d.set(n, v);
            }
        }
        d
    }

    unsafe fn try_header(&mut self) -> Result<()> {
        if self.header || self.outs.iter().any(|o| !o.ready) {
            return Ok(());
        }
        let mut opts = self.mux_opts();
        chk(ff::avformat_write_header(self.octx, &mut opts.0), "nagłówek pliku wynikowego")?;
        self.header = true;
        let mut q = std::mem::take(&mut self.queue);
        let key = |p: *mut ff::AVPacket, tb: ff::AVRational| {
            let t = if (*p).dts != NOPTS { (*p).dts } else { (*p).pts };
            if t == NOPTS { i64::MIN } else { ff::av_rescale_q(t, tb, TBQ) }
        };
        q.sort_by_key(|(_, p, tb)| key(*p, *tb));
        for (oi, mut p, tb) in q {
            let r = self.write(oi, p, tb);
            ff::av_packet_free(&mut p);
            r?;
        }
        Ok(())
    }

    unsafe fn write(&mut self, oi: usize, pkt: *mut ff::AVPacket, tb: ff::AVRational) -> Result<()> {
        let st = self.outs[oi].st;
        (*pkt).stream_index = (*st).index;
        (*pkt).pos = -1;
        ff::av_packet_rescale_ts(pkt, tb, (*st).time_base);
        chk(ff::av_interleaved_write_frame(self.octx, pkt), "zapis pakietu")?;
        Ok(())
    }

    /// Pakiet do kontenera (po zapisie pakiet jest pusty); przed nagłówkiem — do kolejki.
    unsafe fn emit(&mut self, oi: usize, pkt: *mut ff::AVPacket, tb: ff::AVRational) -> Result<()> {
        if self.outs[oi].kind == b'v' && (*pkt).pts != NOPTS {
            self.out_us = self.out_us.max(ff::av_rescale_q((*pkt).pts, tb, TBQ));
        }
        if !self.header {
            let c = ff::av_packet_clone(pkt);
            ff::av_packet_unref(pkt);
            if c.is_null() {
                bail!("brak pamięci na pakiet");
            }
            self.queue.push((oi, c, tb));
            return self.try_header();
        }
        self.write(oi, pkt, tb)
    }

    fn progress(&mut self, end: bool) {
        let Some(path) = &self.job.progress else { return };
        if !end && self.last_progress.elapsed().as_millis() < 500 {
            return;
        }
        self.last_progress = Instant::now();
        let secs = self.started.elapsed().as_secs_f64().max(0.001);
        let speed = self.out_us as f64 / 1e6 / secs;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = write!(
                f,
                "frame={}\nfps={:.1}\nout_time_us={}\nspeed={:.2}x\nprogress={}\n",
                self.frames,
                self.frames as f64 / secs,
                self.out_us.max(0),
                speed,
                if end { "end" } else { "continue" }
            );
        }
    }

    // ---------------------------------------------------------------------------------- wideo

    unsafe fn video(&mut self, oi: usize) -> &mut Video {
        match &mut self.outs[oi].path {
            Path::Video(v) => v,
            _ => unreachable!(),
        }
    }

    unsafe fn video_graph(&mut self, oi: usize, frame: *mut ff::AVFrame) -> Result<()> {
        let job = self.job;
        let kidx = self.outs[oi].kidx;
        let device = job.filter_device.as_deref().and_then(|n| self.devices.by_name(n));
        if job.filter_device.is_some() && device.is_none() {
            bail!("urządzenie filtrów „{}” nie istnieje", job.filter_device.as_deref().unwrap_or(""));
        }
        let pix_opt = job.opts_for(b'v', kidx).iter().rev().find(|o| o.0 == "pix_fmt").map(|o| o.1.to_string());
        let v = self.video(oi);
        let graph = ff::avfilter_graph_alloc();
        v.graph = graph;
        let src = ff::avfilter_graph_alloc_filter(graph, ff::avfilter_get_by_name(cs("buffer").as_ptr()), cs("in").as_ptr());
        let par = ff::av_buffersrc_parameters_alloc();
        (*par).format = (*frame).format;
        (*par).time_base = v.in_tb;
        (*par).width = (*frame).width;
        (*par).height = (*frame).height;
        (*par).sample_aspect_ratio = (*frame).sample_aspect_ratio;
        (*par).frame_rate = v.in_fr;
        (*par).color_space = (*frame).colorspace;
        (*par).color_range = (*frame).color_range;
        if !(*frame).hw_frames_ctx.is_null() {
            (*par).hw_frames_ctx = (*frame).hw_frames_ctx;
        }
        let r = ff::av_buffersrc_parameters_set(src, par);
        ff::av_free(par as *mut c_void);
        chk(r, "parametry źródła filtrów")?;
        chk(ff::avfilter_init_dict(src, null_mut()), "źródło filtrów")?;
        v.src = src;

        // Format wyjściowy: wskazany w zadaniu albo jeden z przyjmowanych przez koder.
        let fmts = match pix_opt {
            Some(p) => p,
            None => supported::<i32>(v.codec, ff::AVCodecConfig::AV_CODEC_CONFIG_PIX_FORMAT)
                .iter()
                .map(|f| cstr(ff::av_get_pix_fmt_name(std::mem::transmute::<i32, ff::AVPixelFormat>(*f))))
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("|"),
        };
        let mut desc = job.vf.clone().unwrap_or_else(|| "null".into());
        if !fmts.is_empty() {
            desc.push_str(&format!(",format=pix_fmts={fmts}"));
        }
        let sink = build_graph(graph, src, "buffersink", &desc, device)?;
        self.video(oi).sink = sink;
        Ok(())
    }

    unsafe fn video_encoder(&mut self, oi: usize, frame: *mut ff::AVFrame) -> Result<()> {
        let job = self.job;
        let (kidx, st, ist) = (self.outs[oi].kidx, self.outs[oi].st, self.outs[oi].ist);
        let global_header = (*(*self.octx).oformat).flags & ff::AVFMT_GLOBALHEADER != 0;
        let v = self.video(oi);
        let codec = v.codec;
        let enc = ff::avcodec_alloc_context3(codec);
        v.enc = enc;
        let (sink, dec, in_fr) = (v.sink, v.dec, v.in_fr);
        (*enc).width = (*frame).width;
        (*enc).height = (*frame).height;
        (*enc).pix_fmt = std::mem::transmute::<i32, ff::AVPixelFormat>((*frame).format);
        (*enc).sample_aspect_ratio = (*frame).sample_aspect_ratio;
        (*enc).time_base = ff::av_buffersink_get_time_base(sink);
        let mut fr = ff::av_buffersink_get_frame_rate(sink);
        if fr.num <= 0 || fr.den <= 0 {
            fr = in_fr;
        }
        if fr.num > 0 && fr.den > 0 {
            (*enc).framerate = fr;
            // Jak ffmpeg: czas kodera liczony w klatkach (1/fps) — równe odstępy także wtedy, gdy kontener
            // źródła zaokrągla czas do milisekund (MKV).
            if q2d(fr) <= 1000.0 {
                (*enc).time_base = ff::AVRational { num: fr.den, den: fr.num };
            }
        }
        (*enc).color_range = (*frame).color_range;
        (*enc).color_primaries = (*frame).color_primaries;
        (*enc).color_trc = (*frame).color_trc;
        (*enc).colorspace = (*frame).colorspace;
        (*enc).chroma_sample_location = (*frame).chroma_location;
        let desc = ff::av_pix_fmt_desc_get((*enc).pix_fmt);
        if !dec.is_null() && !desc.is_null() && (*dec).bits_per_raw_sample > 0 {
            (*enc).bits_per_raw_sample = (*dec).bits_per_raw_sample.min((*desc).comp[0].depth);
        }
        if !(*frame).hw_frames_ctx.is_null() {
            (*enc).hw_frames_ctx = ff::av_buffer_ref((*frame).hw_frames_ctx);
        } else {
            let mut i = 0;
            loop {
                let cfg = ff::avcodec_get_hw_config(codec, i);
                if cfg.is_null() {
                    break;
                }
                if (*cfg).methods & HW_DEVICE_CTX != 0 {
                    if let Some(dev) = self.devices.by_type((*cfg).device_type) {
                        (*enc).hw_device_ctx = ff::av_buffer_ref(dev);
                        break;
                    }
                }
                i += 1;
            }
        }
        // Metadane HDR10 (wyświetlacz wzorcowy, poziomy światła) z klatek źródła → koder i kontener.
        for i in 0..(*frame).nb_side_data as usize {
            let sd = *(*frame).side_data.add(i);
            let d = ff::av_frame_side_data_desc((*sd).type_);
            if !d.is_null() && (*d).props & 1 != 0 {
                ff::av_frame_side_data_clone(&mut (*enc).decoded_side_data, &mut (*enc).nb_decoded_side_data, sd, 0);
            }
        }
        if global_header {
            (*enc).flags |= ff::AV_CODEC_FLAG_GLOBAL_HEADER as c_int;
        }
        let mut tag: Option<String> = None;
        let mut opts = Dict::new();
        let mut explicit: Vec<String> = vec![];
        for (n, val, exp) in job.opts_for(b'v', kidx) {
            match n {
                "pix_fmt" | "disposition" => {}
                // Znaczniki koloru pochodzą z klatek po filtrach (źródło albo wynik mapowania tonów) —
                // tak samo jak w ffmpeg, gdzie te opcje nie nadpisują właściwości klatek.
                "color_primaries" | "color_trc" | "colorspace" | "color_range" => {}
                "tag" => tag = Some(val.to_string()),
                _ if exp || has_opt(ff::avcodec_get_class(), n, None) || has_opt((*codec).priv_class, n, None) => {
                    opts.set(n, val);
                    if exp {
                        explicit.push(n.to_string());
                    }
                }
                _ => {}
            }
        }
        let name = cstr((*codec).name);
        chk(ff::avcodec_open2(enc, codec, &mut opts.0), &format!("koder {name}"))?;
        if let Some((k, _)) = opts.entries().into_iter().find(|(k, _)| explicit.contains(k)) {
            bail!("koder {name} nie ma opcji „{k}”");
        }
        chk(ff::avcodec_parameters_from_context((*st).codecpar, enc), "parametry kodera")?;
        (*st).time_base = (*enc).time_base;
        (*st).avg_frame_rate = (*enc).framerate;
        (*st).r_frame_rate = (*enc).framerate;
        (*st).sample_aspect_ratio = (*enc).sample_aspect_ratio;
        if let Some(t) = tag.filter(|t| t.len() == 4) {
            let b = t.as_bytes();
            (*(*st).codecpar).codec_tag = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        }
        let _ = ist;

        if let Some(inj) = &mut self.inject {
            if (*enc).extradata_size > 0 {
                inj.headers(std::slice::from_raw_parts((*enc).extradata, (*enc).extradata_size as usize))?;
            }
            if let Some((profile, compat)) = inj.dovi() {
                let fps = if (*enc).framerate.num > 0 { ((*enc).framerate.num as u64, (*enc).framerate.den.max(1) as u64) } else { (24, 1) };
                let level = crate::inject::dovi_level((*enc).width as u32, (*enc).height as u32, fps);
                inj.stats.dv_level = level;
                let mut size = 0usize;
                let rec = av_dovi_alloc(&mut size);
                if rec.is_null() {
                    bail!("brak pamięci na konfigurację Dolby Vision");
                }
                (*rec).dv_version_major = 1;
                (*rec).dv_version_minor = 0;
                (*rec).dv_profile = profile;
                (*rec).dv_level = level;
                (*rec).rpu_present_flag = 1;
                (*rec).el_present_flag = 0;
                (*rec).bl_present_flag = 1;
                (*rec).dv_bl_signal_compatibility_id = compat;
                let par = (*st).codecpar;
                if ff::av_packet_side_data_add(
                    &mut (*par).coded_side_data,
                    &mut (*par).nb_coded_side_data,
                    ff::AVPacketSideDataType::AV_PKT_DATA_DOVI_CONF,
                    rec as *mut c_void,
                    size,
                    0,
                )
                .is_null()
                {
                    ff::av_free(rec as *mut c_void);
                    bail!("nie można dodać konfiguracji Dolby Vision do strumienia");
                }
            }
            inj.stats.width = (*enc).width as u32;
            inj.stats.height = (*enc).height as u32;
        }
        self.outs[oi].ready = true;
        self.try_header()
    }

    /// Pakiety kodera wideo → (wstrzykiwacz DV/HDR10+) → kontener.
    unsafe fn video_packets(&mut self, oi: usize, flush: bool) -> Result<()> {
        let enc = self.video(oi).enc;
        let tb = (*enc).time_base;
        loop {
            let r = ff::avcodec_receive_packet(enc, self.opkt);
            if is_again(r) {
                break;
            }
            chk(r, "kodowanie wideo")?;
            self.frames += 1;
            if self.inject.is_none() {
                self.emit(oi, self.opkt, tb)?;
                continue;
            }
            let data = std::slice::from_raw_parts((*self.opkt).data, (*self.opkt).size as usize);
            let ready = self.inject.as_mut().unwrap().push(data)?;
            let c = ff::av_packet_clone(self.opkt);
            ff::av_packet_unref(self.opkt);
            self.video(oi).props.push_back(c);
            self.injected(oi, ready, tb)?;
        }
        if flush {
            if let Some(inj) = &mut self.inject {
                let ready = inj.flush()?;
                self.injected(oi, ready, tb)?;
            }
        }
        Ok(())
    }

    unsafe fn injected(&mut self, oi: usize, ready: Vec<Vec<u8>>, tb: ff::AVRational) -> Result<()> {
        for payload in ready {
            let mut props = self.video(oi).props.pop_front().context("pakiet bez właściwości")?;
            let mut np = ff::av_packet_alloc();
            let r = ff::av_new_packet(np, payload.len() as c_int);
            if r >= 0 {
                std::ptr::copy_nonoverlapping(payload.as_ptr(), (*np).data, payload.len());
                ff::av_packet_copy_props(np, props);
            }
            ff::av_packet_free(&mut props);
            let res = chk(r, "pakiet z metadanymi").and_then(|_| self.emit(oi, np, tb));
            ff::av_packet_free(&mut np);
            res?;
        }
        Ok(())
    }

    /// Klatki z grafu filtrów → koder. `flush`: koniec strumienia.
    unsafe fn video_drain(&mut self, oi: usize) -> Result<()> {
        loop {
            let sink = self.video(oi).sink;
            let r = ff::av_buffersink_get_frame(sink, self.filt);
            if is_again(r) {
                return Ok(());
            }
            chk(r, "filtry wideo")?;
            let meta = dict_entries((*self.filt).metadata);
            if !meta.is_empty() {
                self.frame_meta = meta;
            }
            if self.video(oi).enc.is_null() {
                self.video_encoder(oi, self.filt)?;
            }
            if self.done {
                ff::av_frame_unref(self.filt);
                continue;
            }
            (*self.filt).pict_type = ff::AVPictureType::AV_PICTURE_TYPE_NONE;
            let filt = self.filt;
            let v = self.video(oi);
            let (sink_tb, enc_tb) = (ff::av_buffersink_get_time_base(v.sink), (*v.enc).time_base);
            if (*filt).pts != NOPTS {
                // Najbliższy takt kodera, zawsze rosnąco.
                let mut pts = ff::av_rescale_q((*filt).pts, sink_tb, enc_tb);
                if v.out_last != NOPTS && pts <= v.out_last {
                    pts = v.out_last + 1;
                }
                v.out_last = pts;
                (*filt).pts = pts;
                (*filt).duration = ff::av_rescale_q((*filt).duration, sink_tb, enc_tb).max(0);
            }
            v.sent += 1;
            let (enc, sent) = (v.enc, v.sent);
            let r = ff::avcodec_send_frame(enc, self.filt);
            ff::av_frame_unref(self.filt);
            chk(r, "kodowanie wideo (klatka)")?;
            if self.job.vframes.is_some_and(|n| sent >= n) {
                self.done = true;
            }
            self.video_packets(oi, false)?;
        }
    }

    /// Zdekodowana klatka (albo klatka ze źródła testowego) → filtry.
    unsafe fn video_in(&mut self, oi: usize, frame: *mut ff::AVFrame) -> Result<()> {
        let v = self.video(oi);
        let mut pts = if (*frame).best_effort_timestamp != NOPTS { (*frame).best_effort_timestamp } else { (*frame).pts };
        if pts == NOPTS {
            let step = if v.in_fr.num > 0 { (1.0 / q2d(v.in_fr) / q2d(v.in_tb).max(1e-12)).round() as i64 } else { 1 };
            pts = if v.last_pts == NOPTS { 0 } else { v.last_pts + step.max(1) };
        }
        v.last_pts = pts;
        (*frame).pts = pts;
        if !(*frame).hw_frames_ctx.is_null() && !v.keep_hw {
            let sw = ff::av_frame_alloc();
            let r = ff::av_hwframe_transfer_data(sw, frame, 0);
            if r >= 0 {
                ff::av_frame_copy_props(sw, frame);
                ff::av_frame_unref(frame);
                ff::av_frame_move_ref(frame, sw);
            }
            let mut sw = sw;
            ff::av_frame_free(&mut sw);
            chk(r, "pobranie klatki z karty")?;
        }
        if self.video(oi).graph.is_null() {
            self.video_graph(oi, frame)?;
        }
        let src = self.video(oi).src;
        let r = ff::av_buffersrc_add_frame_flags(src, frame, 4);
        ff::av_frame_unref(frame);
        chk(r, "filtry wideo (wejście)")?;
        self.video_drain(oi)
    }

    unsafe fn video_packet(&mut self, oi: usize, pkt: *const ff::AVPacket) -> Result<()> {
        let dec = self.video(oi).dec;
        let r = ff::avcodec_send_packet(dec, pkt);
        if r < 0 && r != EAGAIN && r != EOF && r != ff::AVERROR_INVALIDDATA {
            chk(r, "dekodowanie wideo")?;
        }
        loop {
            let r = ff::avcodec_receive_frame(dec, self.frame);
            if is_again(r) {
                return Ok(());
            }
            if r == ff::AVERROR_INVALIDDATA {
                continue;
            }
            chk(r, "dekodowanie wideo")?;
            self.video_in(oi, self.frame)?;
        }
    }

    unsafe fn video_finish(&mut self, oi: usize) -> Result<()> {
        if !self.video(oi).dec.is_null() && !self.done {
            self.video_packet(oi, null())?;
        }
        let v = self.video(oi);
        if v.graph.is_null() || v.src.is_null() {
            bail!("brak klatek wideo na wejściu");
        }
        let src = v.src;
        chk(ff::av_buffersrc_add_frame_flags(src, null_mut(), 0), "filtry wideo (koniec)")?;
        self.video_drain(oi)?;
        let enc = self.video(oi).enc;
        if enc.is_null() {
            bail!("filtry nie oddały żadnej klatki wideo");
        }
        chk(ff::avcodec_send_frame(enc, null()), "kodowanie wideo (koniec)")?;
        self.video_packets(oi, true)
    }

    // ---------------------------------------------------------------------------------- audio

    unsafe fn audio(&mut self, oi: usize) -> &mut Audio {
        match &mut self.outs[oi].path {
            Path::Audio(a) => a,
            _ => unreachable!(),
        }
    }

    /// Graf (konwersja formatu próbek, częstotliwości, układu kanałów) i koder — przy pierwszej ramce.
    unsafe fn audio_init(&mut self, oi: usize, frame: *const ff::AVFrame) -> Result<()> {
        let job = self.job;
        let (kidx, st) = (self.outs[oi].kidx, self.outs[oi].st);
        let global_header = (*(*self.octx).oformat).flags & ff::AVFMT_GLOBALHEADER != 0;
        let a = self.audio(oi);
        let (codec, dec) = (a.codec, a.dec);
        let (sr, fmt) = if frame.is_null() { ((*dec).sample_rate, (*dec).sample_fmt as i32) } else { ((*frame).sample_rate, (*frame).format) };
        let mut layout: ff::AVChannelLayout = std::mem::zeroed();
        ff::av_channel_layout_copy(&mut layout, if frame.is_null() { &(*dec).ch_layout } else { &(*frame).ch_layout });
        if layout.order as i32 == ff::AVChannelOrder::AV_CHANNEL_ORDER_UNSPEC as i32 {
            let n = layout.nb_channels;
            ff::av_channel_layout_default(&mut layout, n);
        }
        let graph = ff::avfilter_graph_alloc();
        a.graph = graph;
        let args = format!(
            "time_base=1/{sr}:sample_rate={sr}:sample_fmt={}:channel_layout={}",
            cstr(ff::av_get_sample_fmt_name(std::mem::transmute::<i32, ff::AVSampleFormat>(fmt))),
            layout_name(&layout)
        );
        let mut src: *mut ff::AVFilterContext = null_mut();
        chk(
            ff::avfilter_graph_create_filter(&mut src, ff::avfilter_get_by_name(cs("abuffer").as_ptr()), cs("in").as_ptr(), cs(&args).as_ptr(), null_mut(), graph),
            "źródło filtrów audio",
        )?;
        a.src = src;

        let mut opts = Dict::new();
        let mut explicit: Vec<String> = vec![];
        let (mut o_fmt, mut o_ac, mut o_ar, mut o_bits) = (None, None, None, None);
        for (n, val, exp) in job.opts_for(b'a', kidx) {
            match n {
                "sample_fmt" => o_fmt = Some(val.to_string()),
                "ac" => o_ac = val.parse::<i32>().ok(),
                "ar" => o_ar = Some(val.to_string()),
                "bits_per_raw_sample" => o_bits = val.parse::<i32>().ok(),
                "disposition" | "tag" => {}
                _ if exp || has_opt(ff::avcodec_get_class(), n, None) || has_opt((*codec).priv_class, n, None) => {
                    opts.set(n, val);
                    if exp {
                        explicit.push(n.to_string());
                    }
                }
                _ => {}
            }
        }
        let mut parts: Vec<String> = vec![];
        let fmts = o_fmt.unwrap_or_else(|| {
            supported::<i32>(codec, ff::AVCodecConfig::AV_CODEC_CONFIG_SAMPLE_FORMAT)
                .iter()
                .map(|f| cstr(ff::av_get_sample_fmt_name(std::mem::transmute::<i32, ff::AVSampleFormat>(*f))))
                .collect::<Vec<_>>()
                .join("|")
        });
        if !fmts.is_empty() {
            parts.push(format!("sample_fmts={fmts}"));
        }
        let rates = o_ar.unwrap_or_else(|| {
            supported::<i32>(codec, ff::AVCodecConfig::AV_CODEC_CONFIG_SAMPLE_RATE).iter().map(|r| r.to_string()).collect::<Vec<_>>().join("|")
        });
        if !rates.is_empty() {
            parts.push(format!("sample_rates={rates}"));
        }
        let layouts = match o_ac {
            Some(n) => {
                let mut l: ff::AVChannelLayout = std::mem::zeroed();
                ff::av_channel_layout_default(&mut l, n);
                layout_name(&l)
            }
            None => supported::<ff::AVChannelLayout>(codec, ff::AVCodecConfig::AV_CODEC_CONFIG_CHANNEL_LAYOUT)
                .iter()
                .map(|l| layout_name(l))
                .collect::<Vec<_>>()
                .join("|"),
        };
        if !layouts.is_empty() {
            parts.push(format!("channel_layouts={layouts}"));
        }
        let desc = if parts.is_empty() { "anull".to_string() } else { format!("aformat={}", parts.join(":")) };
        let sink = build_graph(graph, src, "abuffersink", &desc, None)?;
        self.audio(oi).sink = sink;

        let enc = ff::avcodec_alloc_context3(codec);
        self.audio(oi).enc = enc;
        (*enc).sample_fmt = std::mem::transmute::<i32, ff::AVSampleFormat>(ff::av_buffersink_get_format(sink));
        (*enc).sample_rate = ff::av_buffersink_get_sample_rate(sink);
        chk(ff::av_buffersink_get_ch_layout(sink, &mut (*enc).ch_layout), "układ kanałów")?;
        (*enc).time_base = ff::AVRational { num: 1, den: (*enc).sample_rate };
        if let Some(b) = o_bits {
            (*enc).bits_per_raw_sample = b;
        }
        if global_header {
            (*enc).flags |= ff::AV_CODEC_FLAG_GLOBAL_HEADER as c_int;
        }
        let name = cstr((*codec).name);
        chk(ff::avcodec_open2(enc, codec, &mut opts.0), &format!("koder audio {name}"))?;
        if let Some((k, _)) = opts.entries().into_iter().find(|(k, _)| explicit.contains(k)) {
            bail!("koder {name} nie ma opcji „{k}”");
        }
        // AV_CODEC_CAP_VARIABLE_FRAME_SIZE
        if (*codec).capabilities & (1 << 16) == 0 && (*enc).frame_size > 0 {
            ff::av_buffersink_set_frame_size(sink, (*enc).frame_size as u32);
        }
        chk(ff::avcodec_parameters_from_context((*st).codecpar, enc), "parametry kodera audio")?;
        (*st).time_base = (*enc).time_base;
        self.outs[oi].ready = true;
        self.try_header()
    }

    unsafe fn audio_drain(&mut self, oi: usize, flush: bool) -> Result<()> {
        let (sink, enc) = {
            let a = self.audio(oi);
            (a.sink, a.enc)
        };
        let tb = (*enc).time_base;
        loop {
            let r = ff::av_buffersink_get_frame(sink, self.filt);
            let end = is_again(r);
            if !end {
                chk(r, "filtry audio")?;
                let r = ff::avcodec_send_frame(enc, self.filt);
                ff::av_frame_unref(self.filt);
                chk(r, "kodowanie audio (ramka)")?;
            } else if flush {
                chk(ff::avcodec_send_frame(enc, null()), "kodowanie audio (koniec)")?;
            }
            loop {
                let r = ff::avcodec_receive_packet(enc, self.opkt);
                if is_again(r) {
                    break;
                }
                chk(r, "kodowanie audio")?;
                self.emit(oi, self.opkt, tb)?;
            }
            if end {
                return Ok(());
            }
        }
    }

    unsafe fn audio_packet(&mut self, oi: usize, pkt: *const ff::AVPacket) -> Result<()> {
        let dec = self.audio(oi).dec;
        let in_tb = (*self.ist(self.outs[oi].ist)).time_base;
        let r = ff::avcodec_send_packet(dec, pkt);
        if r < 0 && r != EAGAIN && r != EOF && r != ff::AVERROR_INVALIDDATA {
            chk(r, "dekodowanie audio")?;
        }
        loop {
            let r = ff::avcodec_receive_frame(dec, self.frame);
            if is_again(r) {
                return Ok(());
            }
            if r == ff::AVERROR_INVALIDDATA {
                continue;
            }
            chk(r, "dekodowanie audio")?;
            if self.audio(oi).graph.is_null() {
                self.audio_init(oi, self.frame)?;
            }
            let frame = self.frame;
            let a = self.audio(oi);
            let sr = (*frame).sample_rate.max(1);
            let tb = ff::AVRational { num: 1, den: sr };
            let ts = if (*frame).best_effort_timestamp != NOPTS { (*frame).best_effort_timestamp } else { (*frame).pts };
            let mut pts = if ts != NOPTS { ff::av_rescale_q(ts, in_tb, tb) } else if a.next_pts != NOPTS { a.next_pts } else { 0 };
            // Ciągłość próbek: drobne odchyłki czasu z kontenera (zaokrąglenia) nie przesuwają dźwięku.
            if a.next_pts != NOPTS && (pts - a.next_pts).abs() <= (sr as i64 / 400).max(2) {
                pts = a.next_pts;
            }
            (*frame).pts = pts;
            a.next_pts = pts + (*frame).nb_samples as i64;
            let src = a.src;
            let r = ff::av_buffersrc_add_frame_flags(src, frame, 4);
            ff::av_frame_unref(frame);
            chk(r, "filtry audio (wejście)")?;
            self.audio_drain(oi, false)?;
        }
    }

    unsafe fn audio_finish(&mut self, oi: usize) -> Result<()> {
        self.audio_packet(oi, null())?;
        if self.audio(oi).graph.is_null() {
            // Ścieżka bez żadnej ramki: koder z parametrów strumienia, żeby plik miał komplet ścieżek.
            self.audio_init(oi, null())?;
        }
        let src = self.audio(oi).src;
        chk(ff::av_buffersrc_add_frame_flags(src, null_mut(), 0), "filtry audio (koniec)")?;
        self.audio_drain(oi, true)
    }

    // ---------------------------------------------------------------------------------- napisy

    unsafe fn subs_packet(&mut self, oi: usize, pkt: *mut ff::AVPacket) -> Result<()> {
        let (dec, enc) = match &self.outs[oi].path {
            Path::Subs(s) => (s.dec, s.enc),
            _ => unreachable!(),
        };
        let mut sub: ff::AVSubtitle = std::mem::zeroed();
        let mut got: c_int = 0;
        if ff::avcodec_decode_subtitle2(dec, &mut sub, &mut got, pkt) < 0 || got == 0 {
            return Ok(());
        }
        let ms = ff::AVRational { num: 1, den: 1000 };
        if sub.pts == NOPTS {
            sub.pts = ff::av_rescale_q((*pkt).pts, (*self.ist(self.outs[oi].ist)).time_base, TBQ);
        }
        sub.pts += ff::av_rescale_q(sub.start_display_time as i64, ms, TBQ);
        sub.end_display_time = sub.end_display_time.saturating_sub(sub.start_display_time);
        sub.start_display_time = 0;
        let mut res = Ok(());
        if sub.num_rects > 0 {
            let mut buf = vec![0u8; 64 * 1024];
            let n = ff::avcodec_encode_subtitle(enc, buf.as_mut_ptr(), buf.len() as c_int, &sub);
            if n > 0 {
                let mut np = ff::av_packet_alloc();
                if ff::av_new_packet(np, n) >= 0 {
                    std::ptr::copy_nonoverlapping(buf.as_ptr(), (*np).data, n as usize);
                    (*np).pts = sub.pts;
                    (*np).dts = sub.pts;
                    (*np).duration = ff::av_rescale_q(sub.end_display_time as i64, ms, TBQ);
                    (*np).flags |= 1;
                    res = self.emit(oi, np, TBQ);
                }
                ff::av_packet_free(&mut np);
            }
        }
        ff::avsubtitle_free(&mut sub);
        res
    }

    // ---------------------------------------------------------------------------------- przebieg

    unsafe fn run(&mut self) -> Result<()> {
        for d in &self.job.devices {
            self.devices.init(d)?;
        }
        self.open_input()?;
        self.setup_outputs()?;
        self.try_header()?;

        if self.job.lavfi {
            while !self.done && !self.stopped() {
                let r = ff::av_buffersink_get_frame(self.lavfi_sink, self.frame);
                if is_again(r) {
                    break;
                }
                chk(r, "źródło testowe")?;
                self.video_in(0, self.frame)?;
            }
        } else {
            let start = self.start_time();
            while !self.done {
                if self.stopped() {
                    bail!("przerwano");
                }
                let r = ff::av_read_frame(self.ictx, self.pkt);
                if r == EOF {
                    break;
                }
                chk(r, "odczyt pliku wejściowego")?;
                let si = (*self.pkt).stream_index as usize;
                let tb = (*self.ist(si)).time_base;
                // Jak ffmpeg: czas liczony od początku pliku.
                let off = ff::av_rescale_q(start, TBQ, tb);
                if (*self.pkt).pts != NOPTS {
                    (*self.pkt).pts -= off;
                }
                if (*self.pkt).dts != NOPTS {
                    (*self.pkt).dts -= off;
                }
                for oi in 0..self.outs.len() {
                    if self.outs[oi].ist != si {
                        continue;
                    }
                    match self.outs[oi].kind_path() {
                        0 => {
                            let mut c = ff::av_packet_clone(self.pkt);
                            let r = self.emit(oi, c, tb);
                            ff::av_packet_free(&mut c);
                            r?;
                        }
                        1 => self.video_packet(oi, self.pkt)?,
                        2 => self.audio_packet(oi, self.pkt)?,
                        _ => self.subs_packet(oi, self.pkt)?,
                    }
                }
                ff::av_packet_unref(self.pkt);
                self.progress(false);
            }
        }
        if self.stopped() {
            bail!("przerwano");
        }
        for oi in 0..self.outs.len() {
            match self.outs[oi].kind_path() {
                1 => self.video_finish(oi)?,
                2 => self.audio_finish(oi)?,
                _ => {}
            }
        }
        self.try_header()?;
        if !self.header {
            bail!("nie udało się przygotować wszystkich strumieni wyjściowych");
        }
        chk(ff::av_write_trailer(self.octx), "zakończenie pliku wynikowego")?;
        if !(*self.octx).pb.is_null() && (*(*self.octx).oformat).flags & ff::AVFMT_NOFILE == 0 {
            chk(ff::avio_closep(&mut (*self.octx).pb), "zamknięcie pliku wynikowego")?;
        }
        self.progress(true);
        Ok(())
    }
}

impl Out {
    fn kind_path(&self) -> u8 {
        match self.path {
            Path::Copy => 0,
            Path::Video(_) => 1,
            Path::Audio(_) => 2,
            Path::Subs(_) => 3,
        }
    }
}

impl Drop for Engine<'_> {
    fn drop(&mut self) {
        unsafe {
            for (_, p, _) in &mut self.queue {
                ff::av_packet_free(p);
            }
            for o in &mut self.outs {
                match &mut o.path {
                    Path::Copy => {}
                    Path::Video(v) => {
                        for p in &mut v.props {
                            ff::av_packet_free(p);
                        }
                        ff::avfilter_graph_free(&mut v.graph);
                        ff::avcodec_free_context(&mut v.enc);
                        ff::avcodec_free_context(&mut v.dec);
                        if !v.dec_hw.is_null() {
                            drop(Box::from_raw(v.dec_hw));
                        }
                    }
                    Path::Audio(a) => {
                        ff::avfilter_graph_free(&mut a.graph);
                        ff::avcodec_free_context(&mut a.enc);
                        ff::avcodec_free_context(&mut a.dec);
                    }
                    Path::Subs(s) => {
                        ff::avcodec_free_context(&mut s.enc);
                        ff::avcodec_free_context(&mut s.dec);
                    }
                }
            }
            ff::avfilter_graph_free(&mut self.lavfi);
            if !self.octx.is_null() {
                if !(*self.octx).pb.is_null() && (*(*self.octx).oformat).flags & ff::AVFMT_NOFILE == 0 {
                    ff::avio_closep(&mut (*self.octx).pb);
                }
                ff::avformat_free_context(self.octx);
            }
            ff::avformat_close_input(&mut self.ictx);
            ff::av_frame_free(&mut self.frame);
            ff::av_frame_free(&mut self.filt);
            ff::av_packet_free(&mut self.pkt);
            ff::av_packet_free(&mut self.opkt);
        }
    }
}

/// Wykonuje zadanie w bieżącym wątku. Błąd zawiera przyczynę i ostatnie komunikaty bibliotek.
pub fn run(job: &Job, hooks: Hooks) -> Result<Report> {
    super::init();
    super::log_begin();
    let now = Instant::now();
    let mut e = Engine {
        job,
        ictx: null_mut(),
        octx: null_mut(),
        lavfi: null_mut(),
        lavfi_sink: null_mut(),
        devices: Devices { list: vec![] },
        outs: vec![],
        header: false,
        queue: vec![],
        inject: hooks.inject.map(|c| PacketInjector::new(c.meta)),
        frame: unsafe { ff::av_frame_alloc() },
        filt: unsafe { ff::av_frame_alloc() },
        pkt: unsafe { ff::av_packet_alloc() },
        opkt: unsafe { ff::av_packet_alloc() },
        frames: 0,
        done: false,
        out_us: 0,
        started: now,
        last_progress: now,
        frame_meta: vec![],
        stop: hooks.stop,
    };
    let res = unsafe { e.run() };
    let report = Report {
        frames: e.frames,
        inject: e.inject.take().map(|i| i.stats),
        log: vec![],
        frame_meta: std::mem::take(&mut e.frame_meta),
    };
    // Kontekst kodera i urządzenia zwalniane przed odebraniem logu — ich komunikaty też do niego trafiają.
    drop(e);
    let log = super::log_take();
    match res {
        Ok(()) => Ok(Report { log, ..report }),
        Err(err) => {
            let tail: Vec<&str> = log.iter().rev().take(12).rev().map(|s| s.as_str()).collect();
            if tail.is_empty() {
                Err(err)
            } else {
                Err(anyhow!("{err:#}\n{}", tail.join("\n")))
            }
        }
    }
}
