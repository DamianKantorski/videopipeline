use crate::{
    config::*,
    extract::{self, Extractor},
    model::{hms, Panel},
    probe::{self, ColorProfile},
    proc,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Analysis {
    pub input_file: PathBuf,
    pub filename: String,
    pub stem: String,
    pub codec: String,
    pub has_dovi: bool,
    pub has_hdr10plus: bool,
    pub color: ColorProfile,
    pub pol_audio: Option<u32>,
    pub eng_audio: Option<u32>,
    pub first_audio: Option<u32>,
    pub pol_sub: Option<u32>,
    pub eng_sub: Option<u32>,
    /// np. "crop=1920:800:0:140"
    pub crop: Option<String>,
    /// Które etapy pominięto przy analizie (do wykrycia nieaktualnej analizy po zmianie opcji).
    #[serde(default)]
    pub skipped_crop: bool,
    #[serde(default)]
    pub skipped_dv: bool,
    #[serde(default)]
    pub skipped_hdr10plus: bool,
}

impl Analysis {
    /// Analiza pominęła coś, czego bieżące opcje już wymagają.
    pub fn stale_for(&self, o: &Opts) -> bool {
        (self.skipped_crop && !o.no_crop)
            || (self.skipped_dv && !o.no_dv)
            || (self.skipped_hdr10plus && !o.no_hdr10plus)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
    pub fn load(path: &Path) -> Result<Self> {
        Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
    }
}

/// Pasek postępu odczytu strumienia w oknie analizy: procent od `from` do `to`, czas, prędkość, koniec.
struct ReadProgress<'a> {
    panel: &'a Panel,
    name: &'a str,
    step: &'a str,
    duration: Option<f64>,
    from: f64,
    to: f64,
    started: std::time::Instant,
    last: std::time::Instant,
}

impl ReadProgress<'_> {
    /// `cur`: pozycja w filmie (s), `speed`: krotność czasu rzeczywistego (gdy znana).
    fn show(&mut self, cur: f64, speed: Option<f64>, force: bool) {
        if !force && self.last.elapsed() < std::time::Duration::from_millis(500) {
            return;
        }
        self.last = std::time::Instant::now();
        let speed = speed.or_else(|| {
            let e = self.started.elapsed().as_secs_f64();
            (e > 1.0 && cur > 0.0).then(|| cur / e)
        });
        let sp = speed.map(|x| format!(" · {x:.1}x")).unwrap_or_default();
        let note = match self.duration.filter(|d| *d > 0.0) {
            Some(d) => {
                let frac = (cur / d).clamp(0.0, 1.0);
                let eta = match speed.filter(|x| *x > 0.0) {
                    Some(x) => format!(" · zostało ~{}", hms((d - cur).max(0.0) / x)),
                    None => String::new(),
                };
                format!("Odczyt strumienia wideo: {:.0}% ({} / {}){sp}{eta}", frac * 100.0, hms(cur), hms(d))
            }
            None => format!("Odczyt strumienia wideo: {} (czas trwania nieznany)", hms(cur)),
        };
        let (from, to) = (self.from, self.to);
        let pct = self
            .duration
            .filter(|d| *d > 0.0)
            .map(|d| from + (to - from) * (cur / d).clamp(0.0, 1.0))
            .unwrap_or(from + (to - from) * (1.0 - (-self.started.elapsed().as_secs_f64() / 600.0).exp()));
        self.panel.draw_analysis(self.name, pct, self.step, &note);
    }
}

/// Odczyt HEVC przez biblioteki FFmpeg: pakiety prosto z kontenera, bez osobnego procesu.
#[cfg(feature = "libav")]
fn read_hevc(input: &Path, ex: &mut Extractor, prog: &mut ReadProgress) -> Result<()> {
    crate::log::file_line(&format!("[libav] odczyt HEVC do analizy DV/HDR10+: {}", input.display()));
    crate::libav::hevc_nals(
        input,
        |n| ex.nal(n),
        |t| {
            prog.show(t, None, false);
            !proc::cancelled()
        },
    )
}

/// Odczyt HEVC przez ffmpeg (kopia → Annex B na stdout) — gdy program zbudowano bez bibliotek FFmpeg.
#[cfg(not(feature = "libav"))]
fn read_hevc(input: &Path, ex: &mut Extractor, prog: &mut ReadProgress) -> Result<()> {
    use crate::model::read_progress;
    use anyhow::Context;
    use std::process::Stdio;
    let pf = std::env::temp_dir().join(format!(
        "videopipeline-{}-{}.ffprogress",
        std::process::id(),
        prog.name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect::<String>()
    ));
    let _ = fs::remove_file(&pf);
    let full = crate::log::full();
    let cap = crate::log::Capture::new(&format!("ffmpeg (odczyt HEVC do analizy) — {}", prog.name));
    let mut cmd = crate::proc::command("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", if full { "info" } else { "quiet" }, "-nostats", "-progress"])
        .arg(&pf)
        .arg("-i")
        .arg(input)
        .args(["-map", "0:v:0", "-c:v", "copy", "-bsf:v", "hevc_mp4toannexb", "-f", "hevc", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(crate::log::stdio(&cap));
    if full {
        crate::log::file_line(&format!("$ {}", crate::log::cmdline(&cmd)));
    }
    let mut ff = cmd.spawn().context("nie można uruchomić ffmpeg")?;
    proc::register(ff.id());
    let mut out = ff.stdout.take().context("brak stdout ffmpeg")?;
    let mut own = std::mem::replace(ex, Extractor::new(false, false));
    let reader = std::thread::spawn(move || {
        let r = crate::inject::split_annexb(&mut out, |n| own.nal(&n));
        (own, r)
    });
    while !reader.is_finished() {
        if proc::cancelled() {
            let _ = ff.kill();
        }
        let pr = read_progress(&pf);
        let speed = pr.speed.as_deref().and_then(|v| v.trim_end_matches('x').trim().parse::<f64>().ok());
        prog.show(pr.out_us.map(|u| u / 1e6).unwrap_or(0.0), speed, false);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let (own, r) = reader.join().map_err(|_| anyhow::anyhow!("wątek odczytu HEVC przerwany"))?;
    *ex = own;
    let _ = ff.kill();
    let _ = ff.wait();
    proc::unregister(ff.id());
    if let Some(c) = cap {
        c.finish();
    }
    let _ = fs::remove_file(&pf);
    r
}

/// Dolby Vision RPU i HDR10+ z jednego odczytu strumienia HEVC źródła — do plików `rpu_out`
/// (jak dovi_tool extract-rpu) i `hdr10plus_out` (JSON jak hdr10plus_tool extract).
#[allow(clippy::too_many_arguments)]
fn extract_hdr(
    input: &Path,
    rpu_out: Option<&Path>,
    hdr10plus_out: Option<&Path>,
    duration: Option<f64>,
    panel: &Panel,
    name: &str,
    step: &str,
    from: f64,
    to: f64,
) {
    let now = std::time::Instant::now();
    let mut prog = ReadProgress { panel, name, step, duration, from, to, started: now, last: now - std::time::Duration::from_secs(1) };
    let mut ex = Extractor::new(rpu_out.is_some(), hdr10plus_out.is_some());
    let r = read_hevc(input, &mut ex, &mut prog);
    if proc::cancelled() {
        return;
    }
    if let Err(e) = &r {
        // Odczyt urwany (np. uszkodzony koniec pliku) — to, co zebrane, nadal jest używane.
        plog!("    UWAGA: odczyt strumienia HEVC przerwany: {e:#}");
    }
    let c = ex.finish();
    match extract::save(&c, rpu_out, hdr10plus_out) {
        Ok((rpu, h10)) => {
            plog!("    Odczyt HEVC: {} klatek · RPU Dolby Vision: {rpu} · HDR10+: {h10}", c.frames);
            crate::log::file_line(&format!(
                "[analiza] {name}: klatek {}, RPU {rpu} (zebrane {}), HDR10+ {h10} (zebrane {}), {:.1} s",
                c.frames,
                c.rpus.len(),
                c.hdr10plus.len(),
                now.elapsed().as_secs_f64()
            ));
        }
        Err(e) => plog!("    BŁĄD zapisu metadanych DV/HDR10+: {e:#}"),
    }
}

fn last_crop(stderr: &str) -> Option<String> {
    stderr
        .lines()
        .filter_map(|l| {
            let s = &l[l.rfind("crop=")? + 5..];
            let tok = s.split_whitespace().next()?;
            let nums: Vec<&str> = tok.split(':').collect();
            (nums.len() == 4 && nums.iter().all(|n| n.parse::<u32>().is_ok()))
                .then(|| tok.to_string())
        })
        .last()
}

fn detect_crop(
    input: &Path,
    duration: Option<f64>,
    orig_w: u32,
    name: &str,
    panel: &Panel,
) -> Option<String> {
    // Próbki od 15% do 85% długości filmu. Długość nieznana — założenie 2 h (jak dawniej).
    // Krótkie pliki (np. klipy z aparatu) też — wcześniej poniżej 10 minut punkty wypadały za końcem.
    let dur = duration.filter(|d| *d > 1.0).unwrap_or(7200.0);
    let points: Vec<String> = (15..=85).step_by(10).map(|p| format!("{:.3}", dur * p as f64 / 100.0)).collect();
    let mut found: Vec<String> = vec![];

    for (i, ts) in points.iter().enumerate() {
        let pct = 65.0 + 27.0 * (i + 1) as f64 / points.len() as f64;
        panel.draw_analysis(
            name,
            pct,
            "CROPDETECT",
            &format!("Próbka {}/{} — ffmpeg -ss {ts}", i + 1, points.len()),
        );
        if proc::cancelled() {
            break;
        }
        let Some(out) = proc::output(
            crate::proc::command("ffmpeg")
                .args(["-hide_banner", "-ss", ts.as_str(), "-i"])
                .arg(input)
                .args(["-an", "-sn", "-dn", "-vframes", "15", "-vf"])
                .arg("format=yuv420p,gblur=sigma=2,cropdetect=limit=24:round=16:reset=0")
                .args(["-f", "null", "-"]),
        ) else {
            continue;
        };
        if let Some(c) = last_crop(&String::from_utf8_lossy(&out.stderr)) {
            found.push(c);
        }
    }
    if found.is_empty() {
        return None;
    }

    let total = found.len();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for c in &found {
        *counts.entry(c.clone()).or_default() += 1;
    }

    // Największy kadr, który ma >= 90% szerokości i >= 10% głosów.
    let mut best: Option<(u64, u64, String)> = None;
    for (c, n) in &counts {
        let p: Vec<u64> = c.split(':').filter_map(|x| x.parse().ok()).collect();
        if p.len() != 4 {
            continue;
        }
        if p[0] as f64 >= orig_w as f64 * 0.90 && (*n as f64 / total as f64) >= 0.10 {
            let area = p[0] * p[1];
            if best.as_ref().map_or(true, |b| area > b.0) {
                best = Some((area, p[1], c.clone()));
            }
        }
    }
    let (_, h, c) = best?;
    (h < 2140).then(|| format!("crop={c}"))
}

pub fn analyze(opts: &Opts, input: &Path, panel: &mut Panel) -> Result<Analysis> {
    let filename = input.file_name().unwrap().to_string_lossy().to_string();
    let stem = input.file_stem().unwrap().to_string_lossy().to_string();
    let p = opts.paths_for(input);

    panel.draw_analysis(&filename, 8.0, "AUDIO / NAPISY", "Odczytuję ścieżki audio i napisów");
    let media = probe::probe(input)?;

    let mut pol_audio: Option<u32> = None;
    let mut eng_audio: Option<u32> = None;
    for (idx, lang) in &media.audio {
        let l = lang.to_lowercase();
        if l.contains("pol") && pol_audio.is_none() {
            pol_audio = Some(*idx);
        } else if l.contains("eng") && eng_audio.is_none() {
            eng_audio = Some(*idx);
        }
    }
    let mut pol_sub: Option<u32> = None;
    let mut eng_sub: Option<u32> = None;
    for (idx, lang) in &media.subs {
        let l = lang.to_lowercase();
        if l.contains("pol") && pol_sub.is_none() {
            pol_sub = Some(*idx);
        } else if l.contains("eng") && eng_sub.is_none() {
            eng_sub = Some(*idx);
        }
    }

    panel.draw_analysis(&filename, 20.0, "PROFIL KOLORU", "Odczytuję głębię bitową i metadane");
    let color = media.color.clone();
    let (mut has_dovi, mut has_hdr10plus) = (false, false);

    if color.is_sdr_8bit {
        panel.draw_analysis(&filename, 60.0, "SDR 8-BIT", "DV RPU i HDR10+ pominięte — źródło SDR 8-bit");
    } else if opts.codec == Codec::Avc {
        let note = if color.is_hdr() {
            "Źródło HDR → AVC 8-bit SDR z mapowaniem tonów; DV RPU i HDR10+ pominięte"
        } else {
            "DV RPU i HDR10+ pominięte — AVC 8-bit (SDR)"
        };
        panel.draw_analysis(&filename, 60.0, "AVC", note);
    } else if opts.codec == Codec::Av1 {
        panel.draw_analysis(&filename, 60.0, "AV1 / HDR", "DV RPU i HDR10+ pominięte — to ścieżka HEVC");
    } else {
        // Dolby Vision i HDR10+ z jednego odczytu pliku (patrz extract_hdr).
        let mut what: Vec<&str> = vec![];
        if opts.no_dv {
            plog!("    Dolby Vision: pominięte (opcja -pdv)");
        } else {
            let _ = fs::remove_file(&p.temp_rpu);
            what.push("DOLBY VISION RPU");
        }
        if opts.no_hdr10plus {
            plog!("    HDR10+: pominięte (opcja -ph)");
        } else {
            let _ = fs::remove_file(&p.temp_hdr10plus);
            what.push("HDR10+");
        }
        if what.is_empty() {
            panel.draw_analysis(&filename, 60.0, "DV / HDR10+", "Pominięte (opcje: bez Dolby Vision i HDR10+)");
        } else {
            let step = what.join(" + ");
            panel.draw_analysis(&filename, 22.0, &step, "Odczyt strumienia wideo…");
            extract_hdr(
                input,
                (!opts.no_dv).then_some(p.temp_rpu.as_path()),
                (!opts.no_hdr10plus).then_some(p.temp_hdr10plus.as_path()),
                media.duration,
                panel,
                &filename,
                &step,
                22.0,
                60.0,
            );
        }
        if !opts.no_dv {
            if fs::metadata(&p.temp_rpu).map(|m| m.len() > 0).unwrap_or(false) {
                has_dovi = true;
            } else {
                let _ = fs::remove_file(&p.temp_rpu);
            }
        }
        if !opts.no_hdr10plus {
            match fs::metadata(&p.temp_hdr10plus) {
                Ok(m) if m.len() > 10 => has_hdr10plus = true,
                Ok(_) => {
                    let _ = fs::remove_file(&p.temp_hdr10plus);
                }
                Err(_) => {}
            }
        }
        plog!(
            "    Dolby Vision: {} · HDR10+: {}",
            if opts.no_dv { "pominięte" } else if has_dovi { "jest" } else { "brak" },
            if opts.no_hdr10plus { "pominięte" } else if has_hdr10plus { "jest" } else { "brak" }
        );
    }

    let crop = if opts.no_crop {
        panel.draw_analysis(&filename, 92.0, "CROPDETECT", "Pominięte (opcja: bez crop)");
        None
    } else if has_dovi || has_hdr10plus {
        panel.draw_analysis(&filename, 92.0, "CROPDETECT", "Pominięte — zachowuję pełny obraz HDR/DV");
        None
    } else {
        detect_crop(input, media.duration, media.width.unwrap_or(3840), &filename, panel)
    };

    Ok(Analysis {
        input_file: input.to_path_buf(),
        filename,
        stem,
        codec: opts.codec.name().into(),
        has_dovi,
        has_hdr10plus,
        color,
        pol_audio,
        eng_audio,
        first_audio: media.audio.first().map(|a| a.0),
        pol_sub,
        eng_sub,
        crop,
        skipped_crop: opts.no_crop,
        skipped_dv: opts.no_dv,
        skipped_hdr10plus: opts.no_hdr10plus,
    })
}
