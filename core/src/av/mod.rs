//! Silnik multimedialny programu na bibliotekach FFmpeg (libavformat, libavcodec, libavfilter, libavutil)
//! wywoływanych wprost z Rusta — bez uruchamiania programów `ffmpeg` / `ffprobe`.
//!
//! Zadanie (`Job`) opisuje się tym samym słownikiem opcji, którego używa FFmpeg (nazwy koderów, filtry,
//! opcje `AVOption`), więc filtry i ustawienia koderów trafiają do bibliotek bez tłumaczenia. Silnik
//! (`engine`) sam otwiera plik, dekoduje (także sprzętowo), prowadzi klatki przez graf filtrów, koduje,
//! kopiuje / przekodowuje audio i napisy, przenosi metadane i rozdziały oraz zapisuje kontener.
use anyhow::{anyhow, bail, Result};
pub use ffmpeg_next::ffi as ff;
use std::{
    cell::RefCell,
    ffi::{c_char, c_int, c_void, CStr, CString},
    path::PathBuf,
    sync::OnceLock,
};

pub mod engine;
pub use engine::{run, Hooks, InjectCfg, Report};

pub(crate) fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

pub(crate) fn cs(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}

/// Opis błędu biblioteki (jak av_err2str).
pub(crate) fn err_str(code: c_int) -> String {
    let mut buf = [0 as c_char; 256];
    unsafe {
        ff::av_strerror(code, buf.as_mut_ptr(), buf.len());
    }
    cstr(buf.as_ptr())
}

pub(crate) fn chk(ret: c_int, what: &str) -> Result<c_int> {
    if ret < 0 {
        Err(anyhow!("{what}: {}", err_str(ret)))
    } else {
        Ok(ret)
    }
}

/// AVDictionary z automatycznym zwalnianiem.
pub(crate) struct Dict(pub *mut ff::AVDictionary);

impl Dict {
    pub fn new() -> Self {
        Dict(std::ptr::null_mut())
    }
    pub fn set(&mut self, k: &str, v: &str) {
        let (k, v) = (cs(k), cs(v));
        unsafe {
            ff::av_dict_set(&mut self.0, k.as_ptr(), v.as_ptr(), 0);
        }
    }
    pub fn entries(&self) -> Vec<(String, String)> {
        dict_entries(self.0)
    }
}

impl Drop for Dict {
    fn drop(&mut self) {
        unsafe { ff::av_dict_free(&mut self.0) }
    }
}

pub(crate) fn dict_entries(d: *const ff::AVDictionary) -> Vec<(String, String)> {
    let mut out = vec![];
    let mut e: *const ff::AVDictionaryEntry = std::ptr::null();
    loop {
        e = unsafe { ff::av_dict_iterate(d, e) };
        if e.is_null() {
            return out;
        }
        unsafe { out.push((cstr((*e).key), cstr((*e).value))) }
    }
}

// ------------------------------------------------------------------------------------------- log

thread_local! {
    /// Komunikaty bibliotek dla zadania działającego w tym wątku (koniec logu trafia do opisu błędu).
    static SINK: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

const AV_LOG_WARNING: c_int = 24;
const AV_LOG_INFO: c_int = 32;

#[cfg(not(windows))]
type VaList = *mut ff::__va_list_tag;
#[cfg(windows)]
type VaList = ff::va_list;

unsafe extern "C" fn log_cb(ptr: *mut c_void, level: c_int, fmt: *const c_char, vl: VaList) {
    if level > AV_LOG_INFO || fmt.is_null() {
        return;
    }
    let mut buf = [0 as c_char; 1024];
    let mut prefix: c_int = 1;
    ff::av_log_format_line2(ptr, level, fmt, vl, buf.as_mut_ptr(), buf.len() as c_int, &mut prefix);
    let line = cstr(buf.as_ptr());
    let line = line.trim_end();
    if line.is_empty() {
        return;
    }
    let taken = SINK
        .try_with(|s| match s.try_borrow_mut() {
            Ok(mut g) => match g.as_mut() {
                Some(v) => {
                    if v.len() >= 600 {
                        v.drain(..200);
                    }
                    v.push(line.to_string());
                    true
                }
                None => false,
            },
            Err(_) => true,
        })
        .unwrap_or(true);
    // Wątki pomocnicze bibliotek (dekodery, filtry) nie mają zadania — ostrzeżenia idą do pełnego logu.
    if !taken && level <= AV_LOG_WARNING && crate::log::full() {
        crate::log::file_line(&format!("[libav] {line}"));
    }
}

/// Zbieranie komunikatów bibliotek w bieżącym wątku.
pub(crate) fn log_begin() {
    let _ = SINK.try_with(|s| *s.borrow_mut() = Some(vec![]));
}

pub(crate) fn log_take() -> Vec<String> {
    SINK.try_with(|s| s.borrow_mut().take()).ok().flatten().unwrap_or_default()
}

/// Jednorazowe przygotowanie bibliotek: własny odbiorca komunikatów (nic nie trafia na stderr / ekran TUI).
pub fn init() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| unsafe {
        ff::av_log_set_callback(Some(log_cb));
    });
}

/// Wersje bibliotek FFmpeg (do logu i `--detect`).
pub fn versions() -> String {
    let v = |x: u32| format!("{}.{}.{}", x >> 16, (x >> 8) & 0xff, x & 0xff);
    unsafe {
        format!(
            "FFmpeg {} — libavformat {} · libavcodec {} · libavfilter {} · libavutil {}",
            cstr(ff::av_version_info()),
            v(ff::avformat_version()),
            v(ff::avcodec_version()),
            v(ff::avfilter_version()),
            v(ff::avutil_version())
        )
    }
}

fn codecs(encoders: bool) -> Vec<String> {
    init();
    let mut out = vec![];
    let mut it: *mut c_void = std::ptr::null_mut();
    loop {
        let c = unsafe { ff::av_codec_iterate(&mut it) };
        if c.is_null() {
            return out;
        }
        if (unsafe { ff::av_codec_is_encoder(c) } != 0) == encoders {
            out.push(cstr(unsafe { (*c).name }));
        }
    }
}

/// Nazwy wszystkich koderów w libavcodec.
pub fn encoders() -> Vec<String> {
    codecs(true)
}

/// Nazwy wszystkich dekoderów w libavcodec.
pub fn decoders() -> Vec<String> {
    codecs(false)
}

/// Czy libavfilter ma filtr o tej nazwie.
pub fn has_filter(name: &str) -> bool {
    !unsafe { ff::avfilter_get_by_name(cs(name).as_ptr()) }.is_null()
}

pub(crate) fn has_opt(class: *const ff::AVClass, name: &str, unit: Option<&str>) -> bool {
    if class.is_null() {
        return false;
    }
    let mut c = class;
    let n = cs(name);
    let u = unit.map(cs);
    // AV_OPT_SEARCH_FAKE_OBJ: obiektem jest wskaźnik na AVClass.
    !unsafe {
        ff::av_opt_find(
            &mut c as *mut _ as *mut c_void,
            n.as_ptr(),
            u.as_ref().map_or(std::ptr::null(), |u| u.as_ptr()),
            0,
            2,
        )
    }
    .is_null()
}

/// Czy koder ma własną opcję `opt` (np. av1_nvenc → highbitdepth).
pub fn encoder_has_option(encoder: &str, opt: &str) -> bool {
    init();
    let c = unsafe { ff::avcodec_find_encoder_by_name(cs(encoder).as_ptr()) };
    !c.is_null() && has_opt(unsafe { (*c).priv_class }, opt, None)
}

/// Czy opcja kodera przyjmuje nazwaną wartość (np. av1_nvenc: tune → "uhq").
pub fn encoder_option_has_value(encoder: &str, opt: &str, value: &str) -> bool {
    init();
    let c = unsafe { ff::avcodec_find_encoder_by_name(cs(encoder).as_ptr()) };
    if c.is_null() {
        return false;
    }
    let class = unsafe { (*c).priv_class };
    if class.is_null() {
        return false;
    }
    let mut cl = class;
    let o = unsafe { ff::av_opt_find(&mut cl as *mut _ as *mut c_void, cs(opt).as_ptr(), std::ptr::null(), 0, 2) };
    if o.is_null() {
        return false;
    }
    let unit = cstr(unsafe { (*o).unit });
    !unit.is_empty() && has_opt(class, value, Some(&unit))
}

// ------------------------------------------------------------------------------------------- zadanie

/// Do których strumieni wyjściowych odnosi się opcja: wszystkie, rodzaj (`v`/`a`/`s`), n-ty danego rodzaju.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Spec {
    All,
    Kind(u8),
    KindIdx(u8, usize),
}

impl Spec {
    fn parse(s: &str) -> Result<Spec> {
        if s.is_empty() {
            return Ok(Spec::All);
        }
        let mut it = s.split(':');
        let k = it.next().unwrap_or("");
        if !matches!(k, "v" | "a" | "s") {
            bail!("nieobsługiwany wybór strumienia „{s}”");
        }
        let k = k.as_bytes()[0];
        match it.next() {
            None => Ok(Spec::Kind(k)),
            Some(n) => Ok(Spec::KindIdx(k, n.parse().map_err(|_| anyhow!("zły numer strumienia w „{s}”"))?)),
        }
    }
    pub fn matches(&self, kind: u8, idx: usize) -> bool {
        match *self {
            Spec::All => true,
            Spec::Kind(k) => k == kind,
            Spec::KindIdx(k, i) => k == kind && i == idx,
        }
    }
}

/// Wybór strumienia wejściowego: numer bezwzględny albo n-ty danego rodzaju.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Map {
    Index(usize),
    KindIdx(u8, usize),
}

#[derive(Default, Debug)]
pub struct Job {
    /// Urządzenia sprzętowe w składni FFmpeg: `typ=nazwa:urządzenie`, `typ=nazwa@źródło`.
    pub devices: Vec<String>,
    pub filter_device: Option<String>,
    /// Wejście: plik albo opis grafu źródłowego libavfilter (`lavfi`).
    pub input: String,
    pub lavfi: bool,
    pub hwaccel: Option<String>,
    pub hwaccel_device: Option<String>,
    pub hwaccel_output: Option<String>,
    pub vdecoder: Option<String>,
    /// Początek odczytu w sekundach (przeskok do najbliższej klatki kluczowej).
    pub ss: Option<f64>,
    pub maps: Vec<Map>,
    pub no_audio: bool,
    pub no_subs: bool,
    pub vf: Option<String>,
    pub vframes: Option<u64>,
    /// Koder albo `copy` dla strumieni wyjściowych.
    pub codecs: Vec<(Spec, String)>,
    /// Pozostałe opcje (koderów i kontenera) — nazwy jak w FFmpeg.
    pub opts: Vec<(String, Spec, String)>,
    /// (cel: "" = plik, "s" = wszystkie strumienie, "s:a:0" = jeden strumień; klucz; wartość)
    pub metadata: Vec<(String, String, String)>,
    pub map_metadata: Option<i32>,
    pub map_chapters: Option<i32>,
    pub format: Option<String>,
    /// Plik wynikowy; "-" z formatem `null` = bez zapisu.
    pub output: String,
    pub progress: Option<PathBuf>,
}

impl Job {
    /// Zadanie z listy opcji w składni FFmpeg (podzbiór używany przez program).
    pub fn parse(args: &[String]) -> Result<Job> {
        let mut j = Job::default();
        let mut seen_input = false;
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if !a.starts_with('-') || a == "-" {
                if !seen_input {
                    bail!("nieoczekiwany argument „{a}” przed wejściem");
                }
                j.output = a.clone();
                i += 1;
                continue;
            }
            let body = &a[1..];
            let (name, spec) = body.split_once(':').unwrap_or((body, ""));
            match name {
                "hide_banner" | "nostats" | "nostdin" | "y" | "dn" => {
                    i += 1;
                    continue;
                }
                "an" => {
                    j.no_audio = true;
                    i += 1;
                    continue;
                }
                "sn" => {
                    j.no_subs = true;
                    i += 1;
                    continue;
                }
                _ => {}
            }
            let val = args.get(i + 1).ok_or_else(|| anyhow!("opcja {a} bez wartości"))?.clone();
            i += 2;
            match name {
                "loglevel" | "v" => {}
                "progress" => j.progress = Some(PathBuf::from(val)),
                "init_hw_device" => j.devices.push(val),
                "filter_hw_device" => j.filter_device = Some(val),
                "f" if !seen_input => j.lavfi = val == "lavfi",
                "f" => j.format = Some(val),
                "i" => {
                    j.input = val;
                    seen_input = true;
                }
                "ss" => j.ss = val.parse().ok(),
                "hwaccel" => j.hwaccel = Some(val),
                "hwaccel_device" => j.hwaccel_device = Some(val),
                "hwaccel_output_format" => j.hwaccel_output = Some(val),
                "c" | "codec" if !seen_input => j.vdecoder = Some(val),
                "c" | "codec" => j.codecs.push((Spec::parse(spec)?, val)),
                "map" => {
                    let m = val.trim_end_matches('?');
                    let rest = m.split_once(':').map(|(_, r)| r).ok_or_else(|| anyhow!("zły -map „{val}”"))?;
                    j.maps.push(match rest.parse::<usize>() {
                        Ok(n) => Map::Index(n),
                        Err(_) => match Spec::parse(rest)? {
                            Spec::KindIdx(k, n) => Map::KindIdx(k, n),
                            Spec::Kind(k) => Map::KindIdx(k, 0),
                            Spec::All => bail!("zły -map „{val}”"),
                        },
                    });
                }
                "vf" | "filter" => j.vf = Some(val),
                "frames" | "vframes" => j.vframes = val.parse().ok(),
                "metadata" => {
                    let (k, v) = val.split_once('=').ok_or_else(|| anyhow!("zły -metadata „{val}”"))?;
                    j.metadata.push((spec.to_string(), k.to_string(), v.to_string()));
                }
                "map_metadata" => j.map_metadata = val.parse().ok(),
                "map_chapters" => j.map_chapters = val.parse().ok(),
                _ => j.opts.push((name.to_string(), Spec::parse(spec)?, val)),
            }
        }
        if !seen_input {
            bail!("brak wejścia");
        }
        if j.output.is_empty() {
            bail!("brak pliku wynikowego");
        }
        Ok(j)
    }

    /// Koder (albo "copy") dla strumienia wyjściowego danego rodzaju — ostatnie pasujące ustawienie.
    pub(crate) fn codec_for(&self, kind: u8, idx: usize) -> Option<&str> {
        self.codecs.iter().rev().find(|(s, _)| s.matches(kind, idx)).map(|(_, c)| c.as_str())
    }

    /// Opcje dla strumienia: (nazwa, wartość, czy podano wprost dla strumienia).
    pub(crate) fn opts_for(&self, kind: u8, idx: usize) -> Vec<(&str, &str, bool)> {
        self.opts
            .iter()
            .filter(|(_, s, _)| s.matches(kind, idx))
            .map(|(n, s, v)| (n.as_str(), v.as_str(), *s != Spec::All))
            .collect()
    }
}

/// Wykonuje zadanie zapisane jako lista opcji; zwraca (sukces, komunikaty bibliotek i błąd).
pub fn run_args(args: &[String]) -> (bool, String) {
    match Job::parse(args).and_then(|j| run(&j, Hooks::default())) {
        Ok(r) => (true, r.log.join("\n")),
        Err(e) => (false, format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sv(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_job() {
        let j = Job::parse(&sv(&[
            "-hide_banner", "-init_hw_device", "vaapi=va:/dev/dri/renderD128", "-filter_hw_device", "va", "-hwaccel", "vaapi",
            "-i", "in.mkv", "-vf", "crop=1920:800:0:140", "-map", "0:v:0", "-map", "0:2", "-c:a:0", "copy", "-map", "0:3",
            "-c:a:1", "eac3", "-b:a:1", "192k", "-ac:a:1", "2", "-metadata:s:a:1", "language=pol", "-c:v", "hevc_nvenc",
            "-preset:v", "p7", "-metadata:s", "vendor_id=", "-map_metadata", "-1", "-map_chapters", "0", "-movflags",
            "+faststart", "-f", "mp4", "-y", "out.mp4.part",
        ]))
        .unwrap();
        assert_eq!(j.devices, vec!["vaapi=va:/dev/dri/renderD128"]);
        assert_eq!(j.maps, vec![Map::KindIdx(b'v', 0), Map::Index(2), Map::Index(3)]);
        assert_eq!(j.codec_for(b'v', 0), Some("hevc_nvenc"));
        assert_eq!(j.codec_for(b'a', 1), Some("eac3"));
        assert_eq!(j.codec_for(b's', 0), None);
        assert_eq!(j.opts_for(b'a', 1), vec![("b", "192k", true), ("ac", "2", true), ("movflags", "+faststart", false)]);
        assert_eq!(j.map_metadata, Some(-1));
        assert_eq!(j.format.as_deref(), Some("mp4"));
        assert_eq!(j.output, "out.mp4.part");
        assert!(Job::parse(&sv(&["-i", "x"])).is_err());
    }

    /// Dwa zadania naraz w osobnych wątkach (jak dwa równoległe kodowania): źródło testowe → koder
    /// wbudowany w libavcodec → plik MKV; wynik musi dać się odczytać z właściwą liczbą klatek.
    #[test]
    fn two_jobs_in_parallel() {
        let dir = std::env::temp_dir();
        let jobs: Vec<_> = (0..2)
            .map(|i| {
                let out = dir.join(format!("videopipeline-test-{}-{i}.mkv", std::process::id()));
                let o = out.clone();
                let h = std::thread::spawn(move || {
                    let args = sv(&[
                        "-f", "lavfi", "-i", "testsrc2=s=320x240:r=25:d=1", "-frames:v", "20", "-c:v", "mpeg4", "-b:v", "500k",
                        "-pix_fmt", "yuv420p", "-metadata", "title=Próba", "-f", "matroska", &o.to_string_lossy(),
                    ]);
                    run(&Job::parse(&args).unwrap(), Hooks::default())
                });
                (out, h)
            })
            .collect();
        for (out, h) in jobs {
            let rep = h.join().unwrap().expect("zadanie silnika");
            assert_eq!(rep.frames, 20);
            let info = crate::probe::inspect_fresh(&out).expect("odczyt wyniku");
            let v = info.first_video().expect("wideo");
            assert_eq!((v.codec.as_str(), v.width, v.height), ("mpeg4", 320, 240));
            assert_eq!(info.tags.get("title").map(String::as_str), Some("Próba"));
            let _ = std::fs::remove_file(out);
        }
    }

    #[test]
    fn reports_errors_and_capabilities() {
        let (ok, err) = run_args(&sv(&["-f", "lavfi", "-i", "testsrc2=d=0.1", "-c:v", "nie_ma_takiego", "-f", "null", "-"]));
        assert!(!ok && err.contains("nie_ma_takiego"), "{err}");
        let (ok, err) = run_args(&sv(&["-f", "lavfi", "-i", "testsrc2=d=0.1", "-c:v", "mpeg4", "-zla_opcja:v", "1", "-f", "null", "-"]));
        assert!(!ok && err.contains("zla_opcja"), "{err}");
        assert!(encoders().iter().any(|e| e == "eac3"));
        assert!(has_filter("cropdetect") && !has_filter("nie_ma_filtra"));
        assert!(encoder_has_option("aac", "aac_coder") && !encoder_has_option("aac", "highbitdepth"));
    }
}
