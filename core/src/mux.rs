use crate::{
    analysis::Analysis,
    config::*,
    copy,
    model::{Panel, State},
    probe, proc,
};
use anyhow::{bail, Result};
use std::{
    collections::HashMap,
    fs,
    path::Path,
};

/// Kodeki audio, które MP4 przyjmuje bez przekodowania.
const MP4_AUDIO: &[&str] = &["aac", "ac3", "eac3", "mp3", "mp2", "opus", "flac", "alac"];
/// Napisy tekstowe — do MP4 jako mov_text. Obrazkowych (PGS, DVD) MP4 nie przyjmie.
const TEXT_SUBS: &[&str] = &["subrip", "srt", "ass", "ssa", "mov_text", "webvtt", "text"];

/// Docelowa postać ścieżki audio ze źródła.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AudioOut {
    Copy,
    Aac,
    Alac,
    Flac,
    Ac3,
    /// Zapasowo dla MP4, gdy kodek źródła nie mieści się w MP4 (DTS, TrueHD, PCM przy „kopiuj”).
    Eac3,
}

/// Źródło o rozdzielczości ponad 16 bitów (do FLAC / ALAC zapisujemy wtedy 24 bity).
fn hi_res(codec: &str) -> bool {
    codec.starts_with("pcm_s24") || codec.starts_with("pcm_s32") || codec.starts_with("pcm_f")
        || matches!(codec, "truehd" | "mlp" | "dts" | "flac" | "alac")
}

/// Co zrobić ze ścieżką audio o kodeku `codec` w kontenerze `container`.
pub(crate) fn audio_out(choice: AudioChoice, container: Container, codec: &str, apple: bool) -> AudioOut {
    let mp4_ok = codec.is_empty() || MP4_AUDIO.contains(&codec);
    let fits = |o: AudioOut| if container == Container::Mp4 && o == AudioOut::Copy && !mp4_ok { AudioOut::Eac3 } else { o };
    // Ten sam kodek co docelowy — kopia (bez ponownej, stratnej kompresji).
    let same = |c: &str| codec == c;
    match choice {
        AudioChoice::Copy => fits(AudioOut::Copy),
        AudioChoice::Aac => if same("aac") { AudioOut::Copy } else { AudioOut::Aac },
        AudioChoice::Alac => if same("alac") { AudioOut::Copy } else { AudioOut::Alac },
        AudioChoice::Flac => if same("flac") { AudioOut::Copy } else { AudioOut::Flac },
        AudioChoice::Ac3 => if same("ac3") { AudioOut::Copy } else { AudioOut::Ac3 },
        AudioChoice::Auto => {
            if apple {
                // iPhone / iPad: AAC (zgodność z urządzeniami Apple); AAC ze źródła — kopia.
                if same("aac") { AudioOut::Copy } else { AudioOut::Aac }
            } else if codec.starts_with("pcm_") {
                // Nieskompresowane PCM: w MKV bezstratnie i ~2× mniejsze (FLAC), w MP4 AAC.
                if container == Container::Mkv { AudioOut::Flac } else { AudioOut::Aac }
            } else {
                fits(AudioOut::Copy)
            }
        }
    }
}

/// Argumenty kodera dla wyjściowej ścieżki audio nr `n` (najlepsze kodery dostępne w ffmpeg z paczki).
fn audio_codec_args(out: AudioOut, n: usize, codec: &str, channels: u32) -> Vec<String> {
    let c = |k: &str| format!("-{k}:a:{n}");
    let mut v: Vec<String> = vec![];
    match out {
        AudioOut::Copy => v.extend([c("c"), "copy".into()]),
        AudioOut::Aac => {
            // Wbudowany koder AAC ffmpeg (libfdk_aac nie może być rozpowszechniany z GPL, AudioToolbox
            // jest tylko na macOS). Jego tryb VBR jest eksperymentalny i słabszy niż CBR, więc najwyższa
            // jakość = wysoka stała przepływność: 160 kb/s na kanał (stereo 320k, 5.1 960k).
            let rate = (160 * channels.max(1)).min(1280);
            v.extend([c("c"), "aac".into(), c("b"), format!("{rate}k")]);
        }
        AudioOut::Alac => {
            v.extend([c("c"), "alac".into(), c("sample_fmt"), if hi_res(codec) { "s32p" } else { "s16p" }.into()]);
        }
        AudioOut::Flac => {
            v.extend([c("c"), "flac".into(), c("compression_level"), "8".into()]);
            if hi_res(codec) {
                v.extend([c("sample_fmt"), "s32".into(), c("bits_per_raw_sample"), "24".into()]);
            } else {
                v.extend([c("sample_fmt"), "s16".into()]);
            }
        }
        AudioOut::Ac3 => {
            // AC-3 (Dolby Digital): najwyżej 5.1; 640 kb/s dla wielokanałowego, 448 kb/s dla stereo.
            let rate = if channels > 2 { "640k" } else { "448k" };
            v.extend([c("c"), "ac3".into(), c("b"), rate.into()]);
            if channels > 6 {
                v.extend([c("ac"), "6".into()]);
            }
        }
        AudioOut::Eac3 => {
            v.extend([c("c"), "eac3".into(), c("b"), "640k".into()]);
            if channels > 6 {
                v.extend([c("ac"), "6".into()]);
            }
        }
    }
    v
}

fn audio_out_label(o: AudioOut) -> &'static str {
    match o {
        AudioOut::Copy => "kopia",
        AudioOut::Aac => "AAC",
        AudioOut::Alac => "ALAC",
        AudioOut::Flac => "FLAC",
        AudioOut::Ac3 => "AC-3",
        AudioOut::Eac3 => "E-AC-3 640k",
    }
}

/// Metadane globalne z wejścia `src`: wszystkie albo — przy usuwaniu lokalizacji i danych urządzenia —
/// tylko tytuł i data nagrania. Znaczniki strumieni z nazwą producenta / oprogramowania są czyszczone.
pub(crate) fn meta_args(opts: &Opts, a: &Analysis, src: usize) -> Vec<String> {
    if !opts.strip_meta {
        return vec!["-map_metadata".into(), src.to_string()];
    }
    let mut v: Vec<String> = vec!["-map_metadata".into(), "-1".into()];
    let tags = probe::format_tags(&a.input_file);
    for key in ["title", "creation_time"] {
        if let Some(val) = tags.get(key) {
            // FFmpeg 9 skleja powtórzone znaczniki MOV średnikiem („…000000Z;…Z”) — wystarczy pierwszy.
            let val = if key == "creation_time" { val.split(';').next().unwrap_or(val) } else { val };
            v.extend(["-metadata".into(), format!("{key}={val}")]);
        }
    }
    for key in ["handler_name", "vendor_id", "encoder", "com.android.version", "location", "location-eng"] {
        v.extend(["-metadata:s".into(), format!("{key}=")]);
    }
    v
}

/// Mapowanie i kodeki strumieni źródła (wejście ffmpeg nr `src`): wybrane audio i napisy w postaci
/// zgodnej z kontenerem, oraz polska (-pleac3) i angielska (-da) ścieżka E-AC-3 kodowane w locie.
/// Zwraca (argumenty, liczba ścieżek audio).
pub(crate) fn source_streams(
    opts: &Opts,
    a: &Analysis,
    container: Container,
    src: usize,
    info: &HashMap<u32, (String, u32)>,
) -> (Vec<String>, usize) {
    let codec = |idx: u32| info.get(&idx).map(|(c, _)| c.clone()).unwrap_or_default();
    let apple = probe::is_apple_device(&a.input_file);
    let mut args: Vec<String> = vec![];
    let mut n = 0usize;
    for idx in usable_audio(source_audio(opts, a), info) {
        args.extend(["-map".into(), format!("{src}:{idx}")]);
        let c = codec(idx);
        let ch = info.get(&idx).map(|(_, ch)| *ch).unwrap_or(2);
        let out = audio_out(opts.audio, container, &c, apple);
        args.extend(audio_codec_args(out, n, &c, ch));
        if out != AudioOut::Copy {
            plog!("    audio {idx} ({c}, {ch} kan.) → {}", audio_out_label(out));
        }
        n += 1;
    }
    {
        let mut enc = |idx: u32, bitrate: &str, channels: &str, lang: &str| {
            args.extend(["-map".into(), format!("{src}:{idx}")]);
            args.extend([format!("-c:a:{n}"), "eac3".into(), format!("-b:a:{n}"), bitrate.into()]);
            args.extend([format!("-ac:a:{n}"), channels.into()]);
            args.extend([format!("-metadata:s:a:{n}"), format!("language={lang}")]);
            n += 1;
        };
        if let Some(idx) = a.pol_audio.filter(|_| opts.pl_eac3) {
            enc(idx, "192k", "2", "pol");
        }
        // Angielskie audio tylko z -da.
        if let Some(idx) = a.eng_audio.filter(|_| opts.extra) {
            enc(idx, "484k", "6", "eng");
        }
    }
    let mut ns = 0usize;
    for idx in source_subs(opts, a) {
        let c = codec(idx);
        let text = c.is_empty() || TEXT_SUBS.contains(&c.as_str());
        let out_codec = match container {
            Container::Mp4 if !text => {
                plog!("    MP4: pominięto napisy {idx} ({c}) — MP4 przyjmuje tylko napisy tekstowe");
                continue;
            }
            Container::Mp4 => "mov_text",
            Container::Mkv if c == "mov_text" => "srt",
            Container::Mkv => "copy",
        };
        args.extend(["-map".into(), format!("{src}:{idx}"), format!("-c:s:{ns}"), out_codec.into()]);
        ns += 1;
    }
    (args, n)
}

/// Ścieżka audio, której ffmpeg nie potrafi odczytać (np. APAC z iPhone'a w analizie zapisanej
/// przed poprawką) — zamieniana na pierwszą odczytywalną ścieżkę audio źródła.
fn usable_audio(ids: Vec<u32>, info: &HashMap<u32, (String, u32)>) -> Vec<u32> {
    let mut out: Vec<u32> = vec![];
    for idx in ids {
        let codec = info.get(&idx).map(|(c, _)| c.as_str()).unwrap_or("");
        let pick = if probe::decodable(codec) {
            Some(idx)
        } else {
            let mut alt: Vec<u32> = info
                .iter()
                .filter(|(i, (c, ch))| **i != idx && *ch > 0 && !c.is_empty() && probe::decodable(c))
                .map(|(i, _)| *i)
                .collect();
            alt.sort();
            let alt = alt.into_iter().find(|i| !out.contains(i));
            match alt {
                Some(a) => plog!("    audio {idx} ({codec}) bez dekodera w ffmpeg — zamiast niej ścieżka {a}"),
                None => plog!("    audio {idx} ({codec}) bez dekodera w ffmpeg — pominięta"),
            }
            alt
        };
        if let Some(i) = pick.filter(|i| !out.contains(i)) {
            out.push(i);
        }
    }
    out
}

/// Które strumienie źródła kopiujemy bez zmian (wspólne dla MKV i MP4).
fn source_audio(opts: &Opts, a: &Analysis) -> Vec<u32> {
    if let Some(pol) = a.pol_audio {
        // Z -pleac3 polska ścieżka jest dodawana osobno jako E-AC-3.
        if opts.pl_eac3 { vec![] } else { vec![pol] }
    } else if let Some(first) = a.first_audio.filter(|_| !opts.extra || a.eng_audio.is_none()) {
        // Brak ścieżki PL: zachowujemy pierwszą ścieżkę audio (żeby plik nie był niemy).
        vec![first]
    } else {
        vec![]
    }
}

/// Napisy EN i PL tylko z -da.
fn source_subs(opts: &Opts, a: &Analysis) -> Vec<u32> {
    if opts.extra {
        [a.pol_sub, a.eng_sub].iter().flatten().copied().collect()
    } else {
        vec![]
    }
}

/// Pliki tymczasowe i analiza — po gotowym pliku wynikowym nie są już potrzebne.
fn remove_temps(p: &Paths, analysis_file: &Path) {
    for f in [&p.temp_rpu, &p.temp_hdr10plus, &analysis_file.to_path_buf()] {
        let _ = fs::remove_file(f);
    }
}

/// Kopiowanie do source/encoded (tylko z -c) po ukończeniu / przy resume.
fn finish_copy(opts: &Opts, a: &Analysis, output: &Path, panel: &mut Panel) {
    let name = &a.filename;
    if !opts.copy {
        panel.set(name, State::Done, "Gotowe — kopiowanie -c wyłączone");
        return;
    }
    match copy::copy_final_to_source_encoded(&a.input_file, output) {
        Ok(()) => {
            panel.set(name, State::Done, "Kopia w encoded zweryfikowana SHA-512");
            plog!(" -> [{name}] GOTOWE! Kopia w encoded: SHA-512 OK.");
        }
        Err(e) => {
            // Sam plik wynikowy jest poprawny; kolejne uruchomienie z -c ponowi kopię.
            panel.set(name, State::Error, "Kopiowanie do encoded nie powiodło się");
            plog!(" -> [{name}] BŁĄD kopiowania do encoded: {e:#}");
        }
    }
}

/// Etap 3: plik wynikowy powstaje już przy kodowaniu (wprost albo z DV/HDR10+ w locie) — tu tylko
/// sprzątanie plików analizy i kopia do source/encoded (-c).
pub fn run(opts: &Opts, a: &Analysis, analysis_file: &Path, panel: &mut Panel) -> Result<()> {
    let p = opts.paths_for(&a.input_file);
    if proc::cancelled() {
        bail!("przerwano");
    }
    if !p.output.exists() {
        // Kodowanie nie powiodło się (szczegóły w logu etapu 2); analiza zostaje do ponownej próby.
        bail!("kodowanie nie utworzyło pliku wynikowego — szczegóły w logu");
    }
    remove_temps(&p, analysis_file);
    finish_copy(opts, a, &p.output, panel);
    Ok(())
}

#[cfg(test)]
mod audio_tests {
    use super::*;

    #[test]
    fn audio_decisions() {
        use AudioChoice as A;
        use AudioOut as O;
        let (mkv, mp4) = (Container::Mkv, Container::Mp4);
        // jak źródło
        assert_eq!(audio_out(A::Auto, mkv, "pcm_s16le", false), O::Flac);
        assert_eq!(audio_out(A::Auto, mp4, "pcm_s16le", false), O::Aac);
        assert_eq!(audio_out(A::Auto, mkv, "dts", false), O::Copy);
        assert_eq!(audio_out(A::Auto, mp4, "truehd", false), O::Eac3);
        assert_eq!(audio_out(A::Auto, mp4, "pcm_s16le", true), O::Aac);
        assert_eq!(audio_out(A::Auto, mp4, "aac", true), O::Copy);
        // wybrany kodek: ten sam co źródło — kopia
        assert_eq!(audio_out(A::Aac, mkv, "aac", false), O::Copy);
        assert_eq!(audio_out(A::Ac3, mp4, "eac3", false), O::Ac3);
        assert_eq!(audio_out(A::Flac, mp4, "flac", false), O::Copy);
        assert_eq!(audio_out(A::Copy, mp4, "dts", false), O::Eac3);
        assert_eq!(audio_out(A::Copy, mkv, "dts", false), O::Copy);
        // parametry
        assert_eq!(audio_codec_args(O::Aac, 0, "pcm_s16le", 2), ["-c:a:0", "aac", "-b:a:0", "320k"]);
        assert_eq!(audio_codec_args(O::Aac, 1, "dts", 8), ["-c:a:1", "aac", "-b:a:1", "1280k"]);
        assert!(audio_codec_args(O::Flac, 0, "pcm_s24le", 2).contains(&"24".to_string()));
        assert!(audio_codec_args(O::Ac3, 0, "truehd", 8).contains(&"6".to_string()));
    }

    #[test]
    fn undecodable_audio_is_replaced() {
        // Wymaga ffmpeg w PATH (lista kodeków); bez niego nie ma czego sprawdzać.
        if !probe::decodable("aac") || probe::decodable("apple_apac") {
            return;
        }
        // iPhone: 0 wideo, 1 APAC (dźwięk przestrzenny, bez dekodera), 2 PCM stereo
        let info: HashMap<u32, (String, u32)> = [
            (0, ("hevc".to_string(), 0)),
            (1, ("apple_apac".to_string(), 4)),
            (2, ("pcm_s16le".to_string(), 2)),
        ]
        .into();
        assert_eq!(usable_audio(vec![1], &info), [2]);
        assert_eq!(usable_audio(vec![2], &info), [2]);
        assert_eq!(usable_audio(vec![1, 2], &info), [2]);
    }
}
