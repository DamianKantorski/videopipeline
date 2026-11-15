//! Odczyt plików przez libavformat / libavcodec (biblioteki dynamiczne FFmpeg) — zamiast ffprobe.
//! Wynik w tej samej postaci, co z ffprobe (nazwy kodeków, formatów pikseli i kolorów jak w FFmpeg),
//! więc reszta programu nie rozróżnia źródła danych.
use crate::probe::{Info, StreamInfo};
use anyhow::{anyhow, bail, Context, Result};
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

/// Strumień HEVC (pierwsze wideo) pakiet po pakiecie, jako kolejne NAL — bez pośrednika ffmpeg.
/// Najpierw zestawy parametrów z nagłówka kontenera (hvcC), potem dane pakietów: w MKV/MP4 z długościami
/// NAL, w MPEG-TS / surowym HEVC z kodami startowymi. `pos`: pozycja w sekundach po każdym pakiecie;
/// zwrot `false` przerywa odczyt.
pub fn hevc_nals(
    path: &Path,
    mut nal: impl FnMut(&[u8]) -> Result<()>,
    mut pos: impl FnMut(f64) -> bool,
) -> Result<()> {
    init();
    let mut ictx = ff::format::input(path).map_err(|e| anyhow!("libavformat: {e}"))?;
    let st = ictx.streams().find(|s| s.parameters().medium() == ff::media::Type::Video).context("brak strumienia wideo")?;
    let idx = st.index();
    let tb = st.time_base();
    let tb = if tb.1 != 0 { tb.0 as f64 / tb.1 as f64 } else { 0.0 };
    let start = st.start_time();
    let par = st.parameters();
    if par.id() != ff::codec::Id::HEVC {
        bail!("wideo nie jest HEVC");
    }
    let extradata: Vec<u8> = unsafe {
        let p = par.as_ptr();
        if (*p).extradata.is_null() || (*p).extradata_size <= 0 {
            vec![]
        } else {
            std::slice::from_raw_parts((*p).extradata, (*p).extradata_size as usize).to_vec()
        }
    };
    let mut len_size: Option<usize> = None;
    if extradata.len() >= 23 && extradata[0] == 1 {
        // HEVCDecoderConfigurationRecord: rozmiar pola długości NAL i tablice VPS/SPS/PPS/SEI
        len_size = Some((extradata[21] & 3) as usize + 1);
        let mut i = 23;
        for _ in 0..extradata[22] {
            let Some(h) = extradata.get(i..i + 3) else { break };
            let count = u16::from_be_bytes([h[1], h[2]]) as usize;
            i += 3;
            for _ in 0..count {
                let Some(l) = extradata.get(i..i + 2) else { break };
                let l = u16::from_be_bytes([l[0], l[1]]) as usize;
                if let Some(n) = extradata.get(i + 2..i + 2 + l) {
                    nal(n)?;
                }
                i += 2 + l;
            }
        }
    } else {
        for n in crate::inject::annexb_nals(&extradata) {
            nal(n)?;
        }
    }
    for (s, packet) in ictx.packets() {
        if s.index() != idx {
            continue;
        }
        let Some(data) = packet.data() else { continue };
        match len_size {
            Some(ls) => {
                let mut i = 0;
                while i + ls <= data.len() {
                    let l = data[i..i + ls].iter().fold(0usize, |a, b| (a << 8) | *b as usize);
                    i += ls;
                    let Some(n) = data.get(i..i + l) else { break };
                    nal(n)?;
                    i += l;
                }
            }
            None => {
                for n in crate::inject::annexb_nals(data) {
                    nal(n)?;
                }
            }
        }
        if let Some(t) = packet.pts().or(packet.dts()) {
            if !pos(((t - start.max(0).min(t)) as f64 * tb).max(0.0)) {
                bail!("przerwano");
            }
        }
    }
    Ok(())
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
