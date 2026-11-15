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
    path::{Path, PathBuf},
    process::Stdio,
};

fn run_quiet(cmd: &str, args: &[String]) -> bool {
    proc::run(crate::proc::command(cmd).args(args).stdin(Stdio::null())) == Some(0)
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn encode_audio(input: &Path, idx: u32, bitrate: &str, channels: &str, lang: &str, out: &Path) -> bool {
    let args: Vec<String> = [
        "-hide_banner", "-loglevel", if crate::log::full() { "info" } else { "quiet" }, "-i", &s(input), "-map", &format!("0:{idx}"),
        "-c:a", "eac3", "-b:a", bitrate, "-ac", channels, "-metadata:s:a:0",
        &format!("language={lang}"), &s(out),
    ]
    .iter()
    .map(|x| x.to_string())
    .collect();
    run_quiet("ffmpeg", &args)
}

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
/// zgodnej z kontenerem. `encode_audio`: polska (-pleac3) i angielska (-da) ścieżka E-AC-3 kodowane
/// w locie; inaczej dokłada je wołający z plików tymczasowych. Zwraca (argumenty, liczba ścieżek audio).
pub(crate) fn source_streams(
    opts: &Opts,
    a: &Analysis,
    container: Container,
    src: usize,
    encode_audio: bool,
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
    if encode_audio {
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
        // Angielskie audio tylko z -da (jak w ścieżce z mkvmerge).
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

fn mux_mkv(opts: &Opts, a: &Analysis, p: &Paths, current_video: &Path) -> Result<()> {
    let mut cmd: Vec<String> = vec!["--output".into(), s(&p.output), s(current_video)];
    let mut flags: Vec<String> = vec!["--no-video".into(), "--no-buttons".into()];
    if opts.strip_meta {
        flags.extend(["--no-global-tags".into(), "--no-track-tags".into()]);
    }

    // Ścieżki do przekodowania (wybór kodeka audio) — mkvmerge tylko kopiuje, więc najpierw ffmpeg.
    let info = probe::stream_codecs(&a.input_file);
    let apple = probe::is_apple_device(&a.input_file);
    let mut audio: Vec<u32> = vec![];
    let mut converted: Vec<PathBuf> = vec![];
    for idx in usable_audio(source_audio(opts, a), &info) {
        let (c, ch) = info.get(&idx).cloned().unwrap_or_default();
        let out = audio_out(opts.audio, Container::Mkv, &c, apple);
        if out == AudioOut::Copy {
            audio.push(idx);
            continue;
        }
        let f = PathBuf::from(format!("{}.audio{idx}.mka", p.output.display()));
        let mut args: Vec<String> = ["-hide_banner", "-loglevel", if crate::log::full() { "info" } else { "error" }, "-y", "-i"]
            .map(String::from)
            .to_vec();
        args.extend([s(&a.input_file), "-map".into(), format!("0:{idx}")]);
        args.extend(audio_codec_args(out, 0, &c, ch));
        args.push(s(&f));
        plog!("    audio {idx} ({c}, {ch} kan.) → {}", audio_out_label(out));
        if !run_quiet("ffmpeg", &args) || !non_empty(&f) {
            let _ = fs::remove_file(&f);
            for f in &converted {
                let _ = fs::remove_file(f);
            }
            bail!("kodowanie ścieżki audio {idx} do {} nie powiodło się", audio_out_label(out));
        }
        converted.push(f);
    }
    if audio.is_empty() {
        flags.push("--no-audio".into());
    } else {
        let ids: Vec<String> = audio.iter().map(|x| x.to_string()).collect();
        flags.extend(["--audio-tracks".into(), ids.join(",")]);
    }

    let subs = source_subs(opts, a);
    if subs.is_empty() {
        flags.push("--no-subtitles".into());
    } else {
        let ids: Vec<String> = subs.iter().map(|x| x.to_string()).collect();
        flags.extend(["--subtitle-tracks".into(), ids.join(",")]);
    }

    if !audio.is_empty() || !subs.is_empty() {
        cmd.extend(flags);
        cmd.push(s(&a.input_file));
    }
    if opts.pl_eac3 && a.pol_audio.is_some() && p.temp_pol_audio.exists() {
        cmd.extend(["--language".into(), "0:pol".into(), s(&p.temp_pol_audio)]);
    }
    if opts.extra && p.temp_eng_audio.exists() {
        cmd.extend(["--language".into(), "0:eng".into(), s(&p.temp_eng_audio)]);
    }
    for f in &converted {
        cmd.push(s(f));
    }

    // mkvmerge zwraca 1 dla samych ostrzeżeń — to nadal sukces.
    let ok = proc::run(crate::proc::command("mkvmerge").args(&cmd).stdin(Stdio::null())).map_or(false, |c| c <= 1);
    for f in &converted {
        let _ = fs::remove_file(f);
    }
    if !ok {
        bail!("błąd mkvmerge — pliki tymczasowe pozostają, kolejne uruchomienie wznowi proces");
    }
    Ok(())
}

/// MP4: składanie przez ffmpeg (mkvmerge nie zapisuje MP4). Audio i napisy źródła trafiają do MP4
/// w zgodnej postaci (source_streams): kodeki spoza MP4 → E-AC-3, napisy tekstowe → mov_text.
fn mux_mp4(opts: &Opts, a: &Analysis, p: &Paths, video: &Path, dynamic_hdr: bool) -> Result<()> {
    // Surowy strumień HEVC (po dovi_tool/hdr10plus_tool) nie ma znaczników czasu — podajemy fps ze źródła.
    let raw_hevc = video.extension().and_then(|e| e.to_str()) == Some("hevc");

    let mut args: Vec<String> = ["-hide_banner", "-loglevel", "error", "-y"].map(String::from).to_vec();
    if raw_hevc {
        let Some(fps) = probe::video_fps(&a.input_file) else {
            bail!("nie można odczytać liczby klatek na sekundę ze źródła");
        };
        args.extend(["-framerate".into(), fps, "-f".into(), "hevc".into()]);
    }
    args.extend(["-i".into(), s(video), "-i".into(), s(&a.input_file)]);

    let info = probe::stream_codecs(&a.input_file);
    let (src_args, mut out_audio) = source_streams(opts, a, Container::Mp4, 1, false, &info);
    let mut maps: Vec<String> = vec!["-map".into(), "0:v:0".into(), "-c:v".into(), "copy".into()];
    maps.extend(src_args);
    let mut meta: Vec<String> = vec![];
    let mut next_input = 2usize; // 0 = wideo, 1 = źródło

    // E-AC-3 z plików tymczasowych: surowy strumień nie niesie języka, więc ustawiamy go jawnie.
    let mut temp_audio: Vec<(&PathBuf, &str)> = vec![];
    if opts.pl_eac3 && a.pol_audio.is_some() && p.temp_pol_audio.exists() {
        temp_audio.push((&p.temp_pol_audio, "pol"));
    }
    if opts.extra && p.temp_eng_audio.exists() {
        temp_audio.push((&p.temp_eng_audio, "eng"));
    }
    for (path, lang) in temp_audio {
        args.extend(["-i".into(), s(path)]);
        maps.extend(["-map".into(), format!("{next_input}:0"), format!("-c:a:{out_audio}"), "copy".into()]);
        meta.extend([format!("-metadata:s:a:{out_audio}"), format!("language={lang}")]);
        out_audio += 1;
        next_input += 1;
    }

    args.extend(maps);
    args.extend(meta);
    args.extend(["-map_chapters".into(), "1".into()]);
    args.extend(meta_args(opts, a, 1));
    if opts.codec == Codec::Hevc {
        // hvc1: zgodność z odtwarzaczami (Apple, przeglądarki, KDE).
        args.extend(["-tag:v".into(), "hvc1".into()]);
    }
    args.extend(["-movflags".into(), "+faststart".into(), "-f".into(), "mp4".into()]);

    // Zapis do pliku tymczasowego: przerwany mux nie może zostać uznany za gotowy wynik.
    let part = PathBuf::from(format!("{}.part", p.output.display()));
    let _ = fs::remove_file(&part);
    args.push(s(&part));

    if dynamic_hdr {
        plog!("    UWAGA (MP4): RPU Dolby Vision i metadane HDR10+ zostają w strumieniu, ale ffmpeg nie zapisuje konfiguracji DV (dvcC) — część odtwarzaczy pokaże zwykły HDR10.");
    }
    if !run_quiet("ffmpeg", &args) {
        let _ = fs::remove_file(&part);
        bail!("błąd ffmpeg przy składaniu MP4 — pliki tymczasowe pozostają, kolejne uruchomienie wznowi proces");
    }
    fs::rename(&part, &p.output)?;
    Ok(())
}

fn non_empty(p: &Path) -> bool {
    fs::metadata(p).map(|m| m.len() > 0).unwrap_or(false)
}

/// Poprzednia postać wideo po udanym kroku (wstrzyknięcie HDR10+ / Dolby Vision) — od razu z dysku,
/// żeby naraz leżały najwyżej dwie pełne kopie. Wznowienie zaczyna się od ostatniego gotowego kroku.
fn drop_intermediate(prev: &Path) {
    let size = fs::metadata(prev).map(|m| m.len()).unwrap_or(0);
    if fs::remove_file(prev).is_ok() {
        plog!(
            "    Usunięto plik pośredni {} ({:.1} GB)",
            prev.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            size as f64 / 1e9
        );
    }
}

/// Pliki tymczasowe i analiza — po gotowym pliku wynikowym nie są już potrzebne.
fn remove_temps(p: &Paths, analysis_file: &Path) {
    for f in [
        &p.temp_video, &p.step1, &p.step2, &p.temp_rpu, &p.temp_hdr10plus,
        &p.temp_eng_audio, &p.temp_pol_audio, &analysis_file.to_path_buf(),
    ] {
        let _ = fs::remove_file(f);
    }
}

/// Kopiowanie do source/encoded (tylko z -c) po ukończeniu / przy resume.
fn finish_copy(opts: &Opts, a: &Analysis, output: &Path, already: bool, panel: &mut Panel) {
    let name = &a.filename;
    if !opts.copy {
        let note = if already {
            "Plik wynikowy już istnieje — kopiowanie -c wyłączone"
        } else {
            "Gotowe — kopiowanie -c wyłączone"
        };
        panel.set(name, State::Done, note);
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

pub fn run(opts: &Opts, a: &Analysis, analysis_file: &Path, panel: &mut Panel) -> Result<()> {
    let name = &a.filename;
    let p = opts.paths_for(&a.input_file);
    panel.set(name, State::Hdr, "");
    // Opcje wygrywają z wynikiem starej analizy (np. DV wykryte, ale teraz --no-dv).
    let use_dovi = a.has_dovi && !opts.no_dv;
    let use_hdr10plus = a.has_hdr10plus && !opts.no_hdr10plus;
    if proc::cancelled() {
        bail!("przerwano");
    }

    if p.output.exists() {
        // Plik gotowy: zakodowany od razu do kontenera w etapie 2 albo z wcześniejszego uruchomienia.
        let direct = opts.no_temp_video(a.has_dovi, a.has_hdr10plus);
        remove_temps(&p, analysis_file);
        finish_copy(opts, a, &p.output, !direct, panel);
        return Ok(());
    }
    if opts.no_temp_video(a.has_dovi, a.has_hdr10plus)
        && ![&p.temp_video, &p.step1, &p.step2].iter().any(|f| f.exists())
    {
        // Kodowanie wprost do pliku wynikowego nie powiodło się (szczegóły w logu etapu 2).
        bail!("kodowanie nie utworzyło pliku wynikowego — szczegóły w logu");
    }
    plog!(" -> [{name}] Składanie pliku wynikowego ({})...", opts.codec.name());

    // --- wideo + dynamiczne HDR -------------------------------------------
    let current_video: PathBuf = if opts.codec != Codec::Hevc {
        if !p.temp_video.exists() {
            bail!("brak pliku {}", p.temp_video.display());
        }
        panel.set(name, State::Audio, "AV1 — DV/HDR10+ inject pominięte");
        p.temp_video.clone()
    } else {
        let mut cur = if p.step2.exists() {
            plog!("    Wznowienie: znaleziono *_step2.hevc.");
            p.step2.clone()
        } else if p.step1.exists() {
            plog!("    Wznowienie: znaleziono *_step1.hevc.");
            p.step1.clone()
        } else if p.temp_video.exists() {
            p.temp_video.clone()
        } else {
            bail!("brak pliku {}", p.temp_video.display());
        };

        if use_hdr10plus && !p.step2.exists() {
            if p.step1.exists() {
                cur = p.step1.clone();
            } else {
                if !p.temp_hdr10plus.exists() {
                    bail!("brak pliku HDR10+ JSON: {}", p.temp_hdr10plus.display());
                }
                panel.set(name, State::Hdr10Plus, "Wstrzykiwanie HDR10+");
                panel.draw(" ETAP 3/3 — PIPELINE");
                let args = ["inject", "-i", &s(&cur), "-j", &s(&p.temp_hdr10plus), "-o", &s(&p.step1)]
                    .map(String::from);
                if !run_quiet("hdr10plus_tool", &args) || !non_empty(&p.step1) {
                    let _ = fs::remove_file(&p.step1); // niekompletny — nie może udawać gotowego przy wznowieniu
                    bail!("wstrzykiwanie HDR10+ nie powiodło się");
                }
                drop_intermediate(&cur);
                cur = p.step1.clone();
            }
        }
        if use_dovi {
            if p.step2.exists() {
                cur = p.step2.clone();
            } else {
                if !p.temp_rpu.exists() {
                    bail!("brak pliku RPU: {}", p.temp_rpu.display());
                }
                panel.set(name, State::Dv, "Wstrzykiwanie Dolby Vision");
                panel.draw(" ETAP 3/3 — PIPELINE");
                // Skalowanie do 1080p: obszar aktywny (L5) w RPU przeliczony do nowej rozdzielczości.
                let mut rpu_in = p.temp_rpu.clone();
                if let Some(sc) = crate::encode::scale_target(opts, a) {
                    let mut rpus = dolby_vision::rpu::utils::parse_rpu_file(&p.temp_rpu)?;
                    if crate::inject::scale_l5(&mut rpus, sc.factors())? > 0 {
                        rpu_in = p.temp_rpu.with_extension("1080p.bin");
                        crate::inject::write_rpu_file(&rpu_in, &rpus)?;
                    }
                }
                let args = ["inject-rpu", "-i", &s(&cur), "--rpu-in", &s(&rpu_in), "-o", &s(&p.step2)]
                    .map(String::from);
                let ok = run_quiet("dovi_tool", &args) && non_empty(&p.step2);
                if rpu_in != p.temp_rpu {
                    let _ = fs::remove_file(&rpu_in);
                }
                if !ok {
                    let _ = fs::remove_file(&p.step2);
                    bail!("wstrzykiwanie Dolby Vision nie powiodło się");
                }
                drop_intermediate(&cur);
                cur = p.step2.clone();
            }
        }
        panel.set(name, State::Audio, "HDR/DV zakończone lub pominięte");
        cur
    };

    // --- audio -----------------------------------------------------------
    if opts.pl_eac3 {
        if let Some(idx) = a.pol_audio {
            if p.temp_pol_audio.exists() {
                plog!("    Wznowienie: *_pol_192.eac3 już istnieje.");
            } else {
                panel.set(name, State::Audio, "Kompresja audio PL E-AC-3 2.0 192k");
                panel.draw(" ETAP 3/3 — PIPELINE");
                if !encode_audio(&a.input_file, idx, "192k", "2", "pol", &p.temp_pol_audio) {
                    bail!("kompresja polskiej ścieżki E-AC-3 192k nie powiodła się");
                }
            }
        }
    }
    // Angielskie audio kompresujemy i dodajemy tylko z -da.
    if let Some(idx) = a.eng_audio.filter(|_| opts.extra) {
        if p.temp_eng_audio.exists() {
            plog!("    Wznowienie: *_eng.eac3 już istnieje.");
        } else {
            panel.set(name, State::Audio, "Kompresja audio ENG");
            panel.draw(" ETAP 3/3 — PIPELINE");
            if !encode_audio(&a.input_file, idx, "484k", "6", "eng", &p.temp_eng_audio) {
                bail!("kompresja angielskiej ścieżki nie powiodła się");
            }
        }
    }

    // --- składanie kontenera ------------------------------------------------
    panel.set(name, State::Mux, format!("Łączenie strumieni ({})", p.container.ext().to_uppercase()));
    panel.draw(" ETAP 3/3 — PIPELINE");

    match p.container {
        Container::Mp4 => mux_mp4(opts, a, &p, &current_video, use_dovi || use_hdr10plus)?,
        Container::Mkv => mux_mkv(opts, a, &p, &current_video)?,
    }

    // Pliki tymczasowe usuwamy dopiero po udanym złożeniu pliku.
    remove_temps(&p, analysis_file);

    finish_copy(opts, a, &p.output, false, panel);
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
