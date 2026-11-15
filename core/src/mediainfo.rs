//! Informacje o pliku przez bibliotekę MediaInfo (libmediainfo / MediaInfo.dll, ładowana w trakcie
//! działania — dołączona do paczek; bez niej tylko okienko F2 i `--mediainfo` nie działają).
//! `easy` — skrót w stylu widoku „Easy” z MediaInfo GUI (do okienka F2),
//! `full` — pełny raport tekstowy jak `mediainfo <plik>` (tryb skryptowy).
use serde_json::Value;
use std::{
    ffi::{c_char, c_void, CStr, CString},
    path::Path,
    sync::OnceLock,
};

/// Sekcja raportu jak w widoku „Easy” MediaInfo: tytuł i zwięzłe linie.
pub struct Section {
    pub title: String,
    pub lines: Vec<String>,
}

type FnNew = unsafe extern "C" fn() -> *mut c_void;
type FnOpen = unsafe extern "C" fn(*mut c_void, *const c_char) -> usize;
type FnOption = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> *const c_char;
type FnInform = unsafe extern "C" fn(*mut c_void, usize) -> *const c_char;
type FnHandle = unsafe extern "C" fn(*mut c_void);

/// Funkcje C API MediaInfo w wariancie „A” (char*, z CharSet = UTF-8 — także na Windows).
struct Lib {
    _lib: libloading::Library,
    new: FnNew,
    open: FnOpen,
    option: FnOption,
    inform: FnInform,
    close: FnHandle,
    delete: FnHandle,
}

/// Nazwy biblioteki: najpierw obok programu (paczki), potem systemowa.
fn candidates() -> Vec<std::path::PathBuf> {
    let name = if cfg!(windows) { "MediaInfo.dll" } else { "libmediainfo.so.0" };
    let mut v = vec![];
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        v.push(dir.join(name));
        v.push(dir.join("../lib").join(name));
    }
    v.push(name.into());
    if !cfg!(windows) {
        v.push("libmediainfo.so".into());
    }
    v
}

fn lib() -> Result<&'static Lib, String> {
    static LIB: OnceLock<Result<Lib, String>> = OnceLock::new();
    LIB.get_or_init(|| {
        let mut last = String::new();
        for c in candidates() {
            // Ścieżka bez katalogu: wyszukiwanie systemowe (LD_LIBRARY_PATH, rpath, PATH na Windows).
            if c.components().count() > 1 && !c.exists() {
                continue;
            }
            match unsafe { libloading::Library::new(&c) } {
                Ok(l) => unsafe {
                    let sym = |n: &[u8]| -> Result<*const c_void, String> {
                        l.get::<*const c_void>(n).map(|s| *s).map_err(|e| format!("{}: {e}", String::from_utf8_lossy(n)))
                    };
                    let f = (
                        sym(b"MediaInfoA_New\0")?,
                        sym(b"MediaInfoA_Open\0")?,
                        sym(b"MediaInfoA_Option\0")?,
                        sym(b"MediaInfoA_Inform\0")?,
                        sym(b"MediaInfoA_Close\0")?,
                        sym(b"MediaInfoA_Delete\0")?,
                    );
                    let lib = Lib {
                        new: std::mem::transmute::<*const c_void, FnNew>(f.0),
                        open: std::mem::transmute::<*const c_void, FnOpen>(f.1),
                        option: std::mem::transmute::<*const c_void, FnOption>(f.2),
                        inform: std::mem::transmute::<*const c_void, FnInform>(f.3),
                        close: std::mem::transmute::<*const c_void, FnHandle>(f.4),
                        delete: std::mem::transmute::<*const c_void, FnHandle>(f.5),
                        _lib: l,
                    };
                    crate::log::file_line(&format!("[mediainfo] biblioteka: {}", c.display()));
                    return Ok(lib);
                },
                Err(e) => last = format!("{e}"),
            }
        }
        Err(format!("brak biblioteki MediaInfo ({}){}", candidates().last().map(|c| c.display().to_string()).unwrap_or_default(),
            if last.is_empty() { String::new() } else { format!(": {last}") }))
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// Raport MediaInfo dla pliku; `output`: "" = tekst jak `mediainfo plik`, "JSON" jak `--Output=JSON`.
fn run(path: &Path, output: &str) -> Result<String, String> {
    let l = lib()?;
    let c = |x: &str| CString::new(x).map_err(|_| "niepoprawna nazwa pliku".to_string());
    let file = c(&path.to_string_lossy())?;
    unsafe {
        let h = (l.new)();
        if h.is_null() {
            return Err("MediaInfo: nie można utworzyć obiektu".into());
        }
        let opt = |k: &str, v: &str| {
            if let (Ok(k), Ok(v)) = (CString::new(k), CString::new(v)) {
                (l.option)(h, k.as_ptr(), v.as_ptr());
            }
        };
        opt("CharSet", "UTF-8");
        opt("Output", output);
        let ok = (l.open)(h, file.as_ptr()) != 0;
        let text = if ok {
            let p = (l.inform)(h, 0);
            (!p.is_null()).then(|| CStr::from_ptr(p).to_string_lossy().into_owned())
        } else {
            None
        };
        (l.close)(h);
        (l.delete)(h);
        let text = text.ok_or_else(|| format!("MediaInfo nie otworzył pliku {}", path.display()))?;
        if crate::log::full() {
            crate::log::file_line(&format!("[mediainfo] {} ({}):\n{text}", path.display(), if output.is_empty() { "tekst" } else { output }));
        }
        Ok(text)
    }
}

/// Pełny raport MediaInfo (ten sam tekst, co `mediainfo plik`).
pub fn full(path: &Path) -> Result<String, String> {
    run(path, "")
}

/// Skrócony raport do okienka F2.
pub fn easy(path: &Path) -> Result<Vec<Section>, String> {
    let json = run(path, "JSON")?;
    let v: Value = serde_json::from_str(&json).map_err(|e| format!("niepoprawny JSON z mediainfo: {e}"))?;
    let tracks = v
        .pointer("/media/track")
        .and_then(Value::as_array)
        .ok_or("mediainfo nie zwrócił ścieżek (to nie jest plik multimedialny?)")?;
    Ok(build(tracks))
}

// ---------------------------------------------------------------- formatowanie ---

fn s<'a>(t: &'a Value, k: &str) -> Option<&'a str> {
    t.get(k).and_then(Value::as_str).map(str::trim).filter(|x| !x.is_empty())
}

fn num(t: &Value, k: &str) -> Option<f64> {
    s(t, k).and_then(|x| x.split(" / ").next()).and_then(|x| x.parse().ok())
}

fn human_size(b: f64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0} {}", U[i])
    } else {
        format!("{v:.2} {}", U[i]).replace('.', ",")
    }
}

fn duration(sec: f64) -> String {
    let t = sec.round() as u64;
    let (h, m, s) = (t / 3600, t / 60 % 60, t % 60);
    if h > 0 {
        format!("{h} h {m:02} min {s:02} s")
    } else if m > 0 {
        format!("{m} min {s:02} s")
    } else {
        format!("{:.3} s", sec).replace('.', ",")
    }
}

fn language(code: &str) -> String {
    let name = match code.to_ascii_lowercase().as_str() {
        "pl" | "pol" => "polski",
        "en" | "eng" => "angielski",
        "de" | "ger" | "deu" => "niemiecki",
        "fr" | "fre" | "fra" => "francuski",
        "es" | "spa" => "hiszpański",
        "it" | "ita" => "włoski",
        "cs" | "cze" | "ces" => "czeski",
        "ru" | "rus" => "rosyjski",
        "uk" | "ukr" => "ukraiński",
        "ja" | "jpn" => "japoński",
        "ko" | "kor" => "koreański",
        "zh" | "chi" | "zho" => "chiński",
        "und" => "nieokreślony",
        _ => return code.to_string(),
    };
    format!("{name} ({code})")
}

fn yes(t: &Value, k: &str) -> bool {
    s(t, k).is_some_and(|v| v.eq_ignore_ascii_case("yes"))
}

/// 1 strumień / 2 strumienie / 5 strumieni.
fn streams_word(n: usize) -> &'static str {
    let (d, h) = (n % 10, n % 100);
    if n == 1 {
        "strumień"
    } else if (2..=4).contains(&d) && !(12..=14).contains(&h) {
        "strumienie"
    } else {
        "strumieni"
    }
}

/// 1509000 → "1 509 kb/s", 12700000 → "12,7 Mb/s".
fn bitrate_easy(bps: f64) -> String {
    if bps >= 10_000_000.0 {
        return format!("{:.1} Mb/s", bps / 1e6).replace('.', ",");
    }
    let kb = (bps / 1000.0).round() as u64;
    let digits = kb.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    format!("{out} kb/s")
}

fn aspect(a: f64) -> String {
    for (r, n) in [(16.0 / 9.0, "16:9"), (4.0 / 3.0, "4:3"), (1.0, "1:1"), (21.0 / 9.0, "21:9")] {
        if (a - r).abs() < 0.01 {
            return n.to_string();
        }
    }
    format!("{a:.2}:1").replace('.', ",")
}

/// Krótki opis formatu: "AVC (High@L4.1)", "HEVC (Main 10@L5.1@High)", "DTS (XLL)".
fn format_short(t: &Value) -> Option<String> {
    let base = s(t, "Format")?.to_string();
    let mut out = base;
    if let Some(c) = s(t, "Format_Commercial_IfAny") {
        if !c.eq_ignore_ascii_case(&out) {
            out = format!("{out} ({c})");
        }
    }
    if let Some(p) = s(t, "Format_Profile") {
        let mut prof = p.to_string();
        if let Some(l) = s(t, "Format_Level") {
            prof.push_str(&format!("@L{l}"));
        }
        if let Some(tier) = s(t, "Format_Tier") {
            prof.push_str(&format!("@{tier}"));
        }
        out.push_str(&format!(" ({prof})"));
    }
    if let Some(f) = s(t, "Format_AdditionalFeatures") {
        out.push_str(&format!(" ({f})"));
    }
    Some(out)
}

fn lang_name(t: &Value) -> Option<String> {
    s(t, "Language").map(|c| language(c).split(" (").next().unwrap_or(c).to_string())
}

fn line(parts: Vec<Option<String>>) -> String {
    parts.into_iter().flatten().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(", ")
}

fn build(tracks: &[Value]) -> Vec<Section> {
    let of = |ty: &str| -> Vec<&Value> { tracks.iter().filter(|t| s(t, "@type") == Some(ty)).collect() };
    let (video, audio, text, menu) = (of("Video"), of("Audio"), of("Text"), of("Menu"));
    let fmts = |v: &[&Value]| v.iter().filter_map(|t| s(t, "Format")).collect::<Vec<_>>().join(" / ");
    let mut out = vec![];

    if let Some(g) = of("General").first() {
        let mut l = vec![];
        let head = line(vec![num(g, "FileSize").map(human_size), num(g, "Duration").map(duration)]);
        l.push(format!("{}: {head}", s(g, "Format").unwrap_or("?")));
        if let Some(title) = s(g, "Title").or_else(|| s(g, "Movie")) {
            l.push(format!("Tytuł: {title}"));
        }
        for (v, kind) in [(&video, "wideo"), (&audio, "audio"), (&text, "napisów")] {
            if !v.is_empty() {
                l.push(format!("{} {} {kind}: {}", v.len(), streams_word(v.len()), fmts(v)));
            }
        }
        if !menu.is_empty() {
            let chapters: usize = menu
                .iter()
                .filter_map(|m| m.get("extra").and_then(Value::as_object).map(|o| o.len()))
                .sum();
            l.push(format!("{} {} menu (rozdziały: {chapters})", menu.len(), streams_word(menu.len())));
        }
        if let Some(b) = num(g, "OverallBitRate") {
            l.push(format!("Bitrate całkowity: {}", bitrate_easy(b)));
        }
        if let Some(d) = s(g, "Encoded_Date").or_else(|| s(g, "Tagged_Date")) {
            l.push(format!("Data kodowania: {d}"));
        }
        if let Some(a) = s(g, "Encoded_Application") {
            l.push(format!("Aplikacja: {a}"));
        }
        if let Some(lib) = s(g, "Encoded_Library") {
            l.push(format!("Biblioteka: {lib}"));
        }
        out.push(Section { title: "Ogólne".into(), lines: l });
    }

    for t in &video {
        let res = match (num(t, "Width"), num(t, "Height")) {
            (Some(w), Some(h)) => Some(format!(
                "{w:.0}×{h:.0}{}",
                num(t, "DisplayAspectRatio").map(|a| format!(" ({})", aspect(a))).unwrap_or_default()
            )),
            _ => None,
        };
        let fps = num(t, "FrameRate").map(|f| {
            let vfr = if s(t, "FrameRate_Mode") == Some("VFR") { " VFR" } else { "" };
            format!("{} kl./s{vfr}", format!("{f:.3}").replace('.', ","))
        });
        let depth = s(t, "BitDepth").map(|b| format!("{b} bit"));
        let hdr = s(t, "HDR_Format").map(|h| {
            let compat = s(t, "HDR_Format_Compatibility").map(|c| format!(" / {c}")).unwrap_or_default();
            format!("HDR: {h}{compat}")
        });
        let main = line(vec![
            lang_name(t),
            num(t, "BitRate").map(bitrate_easy),
            res,
            fps,
            depth,
            format_short(t),
        ]);
        let mut l = vec![main];
        if let Some(h) = hdr {
            l.push(h);
        }
        out.push(Section { title: titled("Wideo", t), lines: l });
    }
    for t in &audio {
        let ch = s(t, "Channels").map(|c| format!("{c} kan."));
        let bits = s(t, "BitDepth").map(|b| format!("{b} bit"));
        let hz = num(t, "SamplingRate").map(|hz| format!("{:.1} kHz", hz / 1000.0).replace('.', ","));
        let mut l = vec![line(vec![lang_name(t), num(t, "BitRate").map(bitrate_easy), hz, bits, ch, format_short(t)])];
        if yes(t, "Default") || yes(t, "Forced") {
            l.push(line(vec![
                yes(t, "Default").then(|| "domyślna".to_string()),
                yes(t, "Forced").then(|| "wymuszona".to_string()),
            ]));
        }
        out.push(Section { title: titled("Audio", t), lines: l });
    }
    for t in &text {
        out.push(Section {
            title: titled("Napisy", t),
            lines: vec![line(vec![
                lang_name(t),
                s(t, "Format").map(str::to_string),
                yes(t, "Forced").then(|| "wymuszone".to_string()),
                yes(t, "Default").then(|| "domyślne".to_string()),
            ])],
        });
    }
    out
}

fn titled(kind: &str, t: &Value) -> String {
    match s(t, "Title") {
        Some(n) => format!("{kind} — {n}"),
        None => kind.to_string(),
    }
}
