//! Odczyt plików przez libavformat / libavcodec (biblioteki dynamiczne FFmpeg) — zamiast ffprobe.
//! Wynik w tej samej postaci, co z ffprobe (nazwy kodeków, formatów pikseli i kolorów jak w FFmpeg),
//! więc reszta programu nie rozróżnia źródła danych.
use crate::probe::{Info, StreamInfo};
use anyhow::{anyhow, Result};
use ffmpeg_next as ff;
use ff::ffi;
use std::{
    collections::HashMap,
    ffi::{CStr, CString},
    path::Path,
    sync::OnceLock,
};

fn init() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let _ = ff::init();
        // Komunikaty bibliotek szłyby na stderr i psuły ekran TUI — błędy wracają jako Result.
        ff::util::log::set_level(ff::util::log::Level::Quiet);
    });
}

fn cstr(p: *const std::os::raw::c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

fn dict(d: ff::DictionaryRef) -> HashMap<String, String> {
    d.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// Wersje bibliotek (do logu): „libavformat 63.1.102 · libavcodec 63.1.102”.
pub fn versions() -> String {
    let v = |x: u32| format!("{}.{}.{}", x >> 16, (x >> 8) & 0xff, x & 0xff);
    unsafe { format!("libavformat {} · libavcodec {}", v(ffi::avformat_version()), v(ffi::avcodec_version())) }
}

/// Strumienie, długość i znaczniki pliku (jak `ffprobe -show_streams -show_format`).
pub fn inspect(path: &Path) -> Result<Info> {
    init();
    let ictx = ff::format::input(path).map_err(|e| anyhow!("libavformat: {e}"))?;
    let duration = (ictx.duration() > 0).then(|| ictx.duration() as f64 / ffi::AV_TIME_BASE as f64);
    let tags = dict(ictx.metadata());
    let mut streams = vec![];
    for st in ictx.streams() {
        let par = st.parameters();
        // Pola, których ffmpeg-next nie udostępnia wprost, z AVCodecParameters.
        let p = unsafe { &*par.as_ptr() };
        let kind = match par.medium() {
            ff::media::Type::Video => "video",
            ff::media::Type::Audio => "audio",
            ff::media::Type::Subtitle => "subtitle",
            ff::media::Type::Data => "data",
            ff::media::Type::Attachment => "attachment",
            _ => "",
        };
        let codec = if p.codec_id == ffi::AVCodecID::AV_CODEC_ID_NONE {
            String::new()
        } else {
            cstr(unsafe { ffi::avcodec_get_name(p.codec_id) })
        };
        let video = kind == "video";
        let pix_fmt = if video && p.format >= 0 {
            cstr(unsafe { ffi::av_get_pix_fmt_name(std::mem::transmute::<i32, ffi::AVPixelFormat>(p.format)) })
        } else {
            String::new()
        };
        // ffprobe pomija nieokreślone wartości koloru — tu tak samo (puste zamiast „unknown”).
        let name_or_empty =
            |s: String| if video && !matches!(s.as_str(), "unknown" | "unspecified" | "reserved") { s } else { String::new() };
        let rate = st.rate();
        streams.push(StreamInfo {
            index: st.index() as u32,
            kind: kind.into(),
            codec,
            channels: if kind == "audio" { p.ch_layout.nb_channels.max(0) as u32 } else { 0 },
            pix_fmt,
            bits_per_raw_sample: if p.bits_per_raw_sample > 0 { p.bits_per_raw_sample.to_string() } else { String::new() },
            color_range: name_or_empty(cstr(unsafe { ffi::av_color_range_name(p.color_range) })),
            color_space: name_or_empty(cstr(unsafe { ffi::av_color_space_name(p.color_space) })),
            color_transfer: name_or_empty(cstr(unsafe { ffi::av_color_transfer_name(p.color_trc) })),
            color_primaries: name_or_empty(cstr(unsafe { ffi::av_color_primaries_name(p.color_primaries) })),
            width: if video { p.width.max(0) as u32 } else { 0 },
            height: if video { p.height.max(0) as u32 } else { 0 },
            r_frame_rate: format!("{}/{}", rate.numerator(), rate.denominator()),
            tags: dict(st.metadata()),
            default: st.disposition().contains(ff::format::stream::Disposition::DEFAULT),
        });
    }
    Ok(Info { streams, duration, tags })
}

/// Czy biblioteka ma dekoder dla kodeka o nazwie jak w ffprobe (np. „dts” → dekoder „dca”).
pub fn decodable(codec: &str) -> bool {
    init();
    let Ok(name) = CString::new(codec) else { return false };
    unsafe {
        let d = ffi::avcodec_descriptor_get_by_name(name.as_ptr());
        !d.is_null() && !ffi::avcodec_find_decoder((*d).id).is_null()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn decoders() {
        assert!(super::decodable("pcm_s16le"));
        assert!(super::decodable("hevc"));
        assert!(!super::decodable("apple_apac"));
        assert!(!super::decodable("nie_ma_takiego"));
    }
}
