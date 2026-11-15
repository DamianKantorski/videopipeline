use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::Path};

/// Opis pliku: strumienie, długość, znaczniki globalne. Z libavformat (funkcja `libav`) albo z jednego
/// wywołania ffprobe — w obu przypadkach nazwy jak w ffprobe.
#[derive(Clone, Debug, Default)]
pub struct Info {
    pub streams: Vec<StreamInfo>,
    pub duration: Option<f64>,
    pub tags: HashMap<String, String>,
}

#[derive(Clone, Debug, Default)]
pub struct StreamInfo {
    pub index: u32,
    /// "video" / "audio" / "subtitle" / "data" / "attachment"
    pub kind: String,
    /// Nazwa kodeka jak w ffprobe ("hevc", "pcm_s16le", "apple_apac"); pusta dla danych bez kodeka.
    pub codec: String,
    pub channels: u32,
    pub pix_fmt: String,
    pub bits_per_raw_sample: String,
    pub color_range: String,
    pub color_space: String,
    pub color_transfer: String,
    pub color_primaries: String,
    pub width: u32,
    pub height: u32,
    /// np. "24000/1001"
    pub r_frame_rate: String,
    pub tags: HashMap<String, String>,
    /// Ścieżka oznaczona jako domyślna.
    pub default: bool,
}

impl Info {
    pub fn first_video(&self) -> Option<&StreamInfo> {
        self.streams.iter().find(|s| s.kind == "video")
    }
}

fn inspect_backend(path: &Path) -> Result<Info> {
    let r = crate::libav::inspect(path);
    if crate::log::full() {
        match &r {
            Ok(i) => crate::log::file_line(&format!(
                "[libav] {} — {} strumieni, długość {:?} s\n{:#?}",
                path.display(),
                i.streams.len(),
                i.duration,
                i
            )),
            Err(e) => crate::log::file_line(&format!("[libav] {} — BŁĄD: {e:#}", path.display())),
        }
    }
    r
}

/// Opis pliku bez pamięci podręcznej (np. plik wynikowy tuż po zapisie).
pub fn inspect_fresh(path: &Path) -> Result<Info> {
    inspect_backend(path)
}

/// Opis pliku, zapamiętany dla (ścieżka, rozmiar, czas zmiany) — zmieniony plik jest czytany na nowo.
pub fn inspect(path: &Path) -> Result<Info> {
    use std::sync::Mutex;
    type Key = (std::path::PathBuf, u64, Option<std::time::SystemTime>);
    static CACHE: Mutex<Option<HashMap<Key, Info>>> = Mutex::new(None);
    let meta = std::fs::metadata(path).ok();
    let key: Key = (path.to_path_buf(), meta.as_ref().map_or(0, |m| m.len()), meta.and_then(|m| m.modified().ok()));
    if let Some(i) = CACHE.lock().ok().and_then(|g| g.as_ref().and_then(|m| m.get(&key).cloned())) {
        return Ok(i);
    }
    let info = inspect_backend(path)?;
    if let Ok(mut g) = CACHE.lock() {
        let m = g.get_or_insert_with(HashMap::new);
        if m.len() > 512 {
            m.clear();
        }
        m.insert(key, info.clone());
    }
    Ok(info)
}

/// Czy ffmpeg z paczki umie zdekodować kodek. Np. dźwięk przestrzenny z iPhone'a (apple_apac) nie ma
/// dekodera — takiej ścieżki nie da się przekodować.
pub fn decodable(codec: &str) -> bool {
    codec.is_empty() || crate::libav::decodable(codec)
}

/// Źródło odczytu informacji o plikach (do logu przed startem).
pub fn backend() -> String {
    crate::av::versions()
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ColorProfile {
    pub pix_fmt: String,
    pub bits_per_raw_sample: String,
    pub primaries: String,
    pub trc: String,
    pub space: String,
    pub range: String,
    pub is_sdr_8bit: bool,
    pub sdr_primaries: String,
    pub sdr_trc: String,
    pub sdr_space: String,
    pub sdr_range: String,
}

impl ColorProfile {
    /// HDR (PQ / HLG) — w odróżnieniu od zwykłego 10-bit SDR.
    pub fn is_hdr(&self) -> bool {
        matches!(self.trc.as_str(), "smpte2084" | "arib-std-b67")
    }

    /// Pełny zakres jasności (0–255), typowy dla aparatów i telefonów (yuvj420p, „pc”). Wynik jest
    /// przeliczany do zakresu ograniczonego (16–235) — część odtwarzaczy ignoruje znacznik pełnego
    /// zakresu w HEVC/AV1 i pokazuje wtedy wyblakły albo przepalony obraz.
    pub fn full_range(&self) -> bool {
        matches!(self.range.as_str(), "pc" | "jpeg") || self.pix_fmt.starts_with("yuvj")
    }
}

pub struct Media {
    pub audio: Vec<(u32, String)>,
    pub subs: Vec<(u32, String)>,
    pub width: Option<u32>,
    pub duration: Option<f64>,
    pub color: ColorProfile,
}

fn known(s: &str) -> bool {
    !s.is_empty() && s != "unknown" && s != "unspecified"
}

fn color_from(s: &StreamInfo) -> ColorProfile {
    let mut c = ColorProfile {
        pix_fmt: s.pix_fmt.clone(),
        bits_per_raw_sample: s.bits_per_raw_sample.clone(),
        primaries: s.color_primaries.clone(),
        trc: s.color_transfer.clone(),
        space: s.color_space.clone(),
        range: s.color_range.clone(),
        sdr_primaries: "bt709".into(),
        sdr_trc: "bt709".into(),
        sdr_space: "bt709".into(),
        sdr_range: "tv".into(),
        ..Default::default()
    };
    let is_8bit = c.bits_per_raw_sample == "8"
        || matches!(
            c.pix_fmt.as_str(),
            "yuv420p" | "yuvj420p" | "nv12" | "yuv422p" | "yuvj422p" | "yuv444p"
                | "yuvj444p" | "gray" | "gray8" | "rgb24" | "bgr24" | "gbrp"
        );
    let is_hdr = matches!(c.trc.as_str(), "smpte2084" | "arib-std-b67");
    c.is_sdr_8bit = is_8bit && !is_hdr;
    if known(&c.primaries) {
        c.sdr_primaries = c.primaries.clone();
    }
    if known(&c.trc) {
        c.sdr_trc = c.trc.clone();
    }
    if known(&c.space) {
        c.sdr_space = c.space.clone();
    }
    // Wynik zawsze w zakresie ograniczonym: pełny zakres źródła jest przeliczany w filtrach
    // (encode.rs: range_fix), więc znacznik „tv” jest prawdziwy.
    c
}

pub fn probe(input: &Path) -> Result<Media> {
    let info = inspect(input).with_context(|| format!("odczyt {}", input.display()))?;
    let lang = |s: &StreamInfo| s.tags.get("language").cloned().unwrap_or_default();

    // Ścieżki audio, które ffmpeg potrafi odczytać; najpierw oznaczona jako domyślna (iPhone: APAC
    // z dźwiękiem przestrzennym jest pierwsza, ale bez dekodera i nie domyślna — domyślne jest PCM).
    let mut audio_streams: Vec<&StreamInfo> = info.streams.iter().filter(|s| s.kind == "audio").collect();
    audio_streams.retain(|s| {
        let ok = decodable(&s.codec);
        if !ok {
            plog!("    audio {} ({}) pominięte — biblioteki FFmpeg nie mają dekodera tego kodeka", s.index, s.codec);
        }
        ok
    });
    audio_streams.sort_by_key(|s| if s.default { 0 } else { 1 });
    let audio = audio_streams.iter().map(|s| (s.index, lang(s))).collect();
    let subs = info.streams.iter().filter(|s| s.kind == "subtitle").map(|s| (s.index, lang(s))).collect();
    let video = info.first_video();

    Ok(Media {
        audio,
        subs,
        width: video.map(|v| v.width).filter(|w| *w > 0),
        duration: info.duration,
        color: video.map(color_from).unwrap_or_default(),
    })
}

/// Kodek i liczba kanałów każdego strumienia (indeks → (kodek, kanały; 0 dla nie-audio)).
/// Do wyboru: kopiować strumień do kontenera czy przekodować (MP4 nie przyjmie np. DTS ani napisów PGS).
pub fn stream_codecs(input: &Path) -> HashMap<u32, (String, u32)> {
    inspect(input)
        .map(|i| i.streams.iter().map(|s| (s.index, (s.codec.clone(), s.channels))).collect())
        .unwrap_or_default()
}

/// Znaczniki globalne pliku (format tags).
pub fn format_tags(path: &Path) -> HashMap<String, String> {
    inspect(path).map(|i| i.tags).unwrap_or_default()
}

/// Nagranie z iPhone'a / iPada (znaczniki QuickTime z aparatu Apple).
pub fn is_apple_device(path: &Path) -> bool {
    let t = format_tags(path);
    t.get("com.apple.quicktime.make").is_some_and(|m| m.trim() == "Apple")
        || t.get("com.apple.quicktime.model").is_some_and(|m| m.starts_with("iPhone") || m.starts_with("iPad"))
}

/// Kodek pierwszego strumienia wideo z komunikatem błędu — do sprawdzenia pliku wynikowego.
pub fn video_codec_diag(path: &Path) -> (Option<String>, String) {
    match inspect_fresh(path) {
        Ok(i) => match i.first_video().map(|v| v.codec.clone()).filter(|c| !c.is_empty()) {
            Some(c) => (Some(c), String::new()),
            None => (None, "brak strumienia wideo".into()),
        },
        Err(e) => (None, format!("{e:#}")),
    }
}

pub fn video_codec(path: &Path) -> Option<String> {
    video_codec_diag(path).0
}

/// Liczba klatek na sekundę pierwszego strumienia wideo, np. "24000/1001".
pub fn video_fps(path: &Path) -> Option<String> {
    let i = inspect(path).ok()?;
    let r = i.first_video()?.r_frame_rate.clone();
    (!r.is_empty() && r != "0/0" && !r.ends_with("/0")).then_some(r)
}

/// Wymiary pierwszego strumienia wideo (w pikselach kodowanych).
pub fn video_size(path: &Path) -> Option<(u32, u32)> {
    let i = inspect(path).ok()?;
    let v = i.first_video()?;
    (v.width > 0 && v.height > 0).then_some((v.width, v.height))
}

pub fn container_duration(path: &Path) -> Option<f64> {
    inspect(path).ok()?.duration
}
