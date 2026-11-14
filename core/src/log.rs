//! Bufor logu (TUI) albo echo na stdout (tryb skryptowy — bez buforowania w RAM).
//! Opcjonalnie pełny log do pliku: każda linia z czasem, każde wywołanie narzędzia z całym wyjściem.
use std::{
    fs::File,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
};

static FILE: Mutex<Option<(BufWriter<File>, PathBuf)>> = Mutex::new(None);
static FILE_ON: AtomicBool = AtomicBool::new(false);

/// Otwiera plik pełnego logu (nadpisuje istniejący).
pub fn open_file(path: &Path) -> std::io::Result<()> {
    let f = File::create(path)?;
    if let Ok(mut g) = FILE.lock() {
        *g = Some((BufWriter::new(f), path.to_path_buf()));
        FILE_ON.store(true, Ordering::SeqCst);
    }
    Ok(())
}

pub fn close_file() -> Option<PathBuf> {
    FILE_ON.store(false, Ordering::SeqCst);
    let mut g = FILE.lock().ok()?;
    let (mut w, p) = g.take()?;
    let _ = w.flush();
    Some(p)
}

/// Czy pełny log jest zapisywany.
pub fn full() -> bool {
    FILE_ON.load(Ordering::Relaxed)
}

fn stamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

/// Surowy tekst do pliku (bez czasu przy każdej linii — wyjście narzędzi).
fn file_raw(text: &str) {
    if !full() {
        return;
    }
    if let Ok(mut g) = FILE.lock() {
        if let Some((w, _)) = g.as_mut() {
            let _ = w.write_all(text.as_bytes());
            if !text.ends_with('\n') {
                let _ = w.write_all(b"\n");
            }
            let _ = w.flush();
        }
    }
}

/// Linia tylko do pliku pełnego logu (z czasem), bez wyświetlania w programie.
pub fn file_line(s: &str) {
    if !full() {
        return;
    }
    let t = stamp();
    let mut out = String::new();
    if s.is_empty() {
        out.push_str(&format!("[{t}]\n"));
    }
    for l in s.lines() {
        out.push_str(&format!("[{t}] {l}\n"));
    }
    file_raw(&out);
}

/// Wiersz polecenia do logu (argumenty ze spacjami w cudzysłowie).
pub fn cmdline(cmd: &Command) -> String {
    let mut v = vec![cmd.get_program().to_string_lossy().into_owned()];
    for a in cmd.get_args() {
        let a = a.to_string_lossy();
        v.push(if a.is_empty() || a.contains([' ', '"', '\'', '|', ';', '&']) { format!("\"{a}\"") } else { a.into_owned() });
    }
    v.join(" ")
}

/// Wywołanie narzędzia z pełnym wyjściem — tylko do pliku.
pub fn tool(cmd: &Command, status: Option<i32>, stdout: &[u8], stderr: &[u8]) {
    if !full() {
        return;
    }
    let mut t = format!("[{}] $ {}\n", stamp(), cmdline(cmd));
    for (name, data) in [("stdout", stdout), ("stderr", stderr)] {
        let s = String::from_utf8_lossy(data);
        let s = s.trim_end();
        if !s.is_empty() {
            t.push_str(&format!("--- {name} ---\n{s}\n"));
        }
    }
    t.push_str(&format!("--- kod wyjścia: {} ---\n", status.map_or("brak (przerwany)".to_string(), |c| c.to_string())));
    file_raw(&t);
}

/// Dołącza do pełnego logu zawartość pliku logu narzędzia (np. ffmpeg kodującego).
pub fn append_file(title: &str, path: &Path) {
    if !full() {
        return;
    }
    let body = std::fs::read(path).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    file_raw(&format!("[{}] === {title} ({}) ===\n{}\n=== koniec: {title} ===", stamp(), path.display(), body.trim_end()));
}

/// Wyjście procesu uruchamianego w tle: plik tymczasowy, gdy pełny log jest włączony (potem
/// `Capture::finish` dołącza go do logu), inaczej /dev/null.
pub struct Capture {
    path: PathBuf,
    title: String,
}

impl Capture {
    pub fn new(title: &str) -> Option<Capture> {
        if !full() {
            return None;
        }
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("videopipeline-{}-cap{n}.log", std::process::id()));
        File::create(&path).ok()?;
        Some(Capture { path, title: title.to_string() })
    }
    pub fn stdio(&self) -> Stdio {
        std::fs::OpenOptions::new().append(true).open(&self.path).map(Stdio::from).unwrap_or_else(|_| Stdio::null())
    }
    pub fn finish(self) {
        append_file(&self.title, &self.path);
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn stdio(c: &Option<Capture>) -> Stdio {
    c.as_ref().map_or_else(Stdio::null, Capture::stdio)
}

/// `Command::output()` z zapisem wywołania i wyjścia do pełnego logu.
pub trait LoggedOutput {
    fn logged_output(&mut self) -> std::io::Result<Output>;
}

impl LoggedOutput for Command {
    fn logged_output(&mut self) -> std::io::Result<Output> {
        let r = self.output();
        if let Ok(o) = &r {
            tool(self, o.status.code(), &o.stdout, &o.stderr);
        }
        r
    }
}

static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
static ECHO: AtomicBool = AtomicBool::new(false);
const MAX_LINES: usize = 500;

/// Tryb skryptowy: linie idą od razu na stdout i nie są przechowywane.
pub fn set_echo(v: bool) {
    ECHO.store(v, Ordering::SeqCst);
}

pub fn push(s: String) {
    file_line(&s);
    if ECHO.load(Ordering::SeqCst) {
        let mut out = std::io::stdout().lock();
        if s.is_empty() {
            let _ = writeln!(out);
        }
        for l in s.lines() {
            let _ = writeln!(out, "{l}");
        }
        return;
    }
    let Ok(mut g) = LOG.lock() else { return };
    if s.is_empty() {
        g.push(String::new());
    }
    for l in s.lines() {
        g.push(l.chars().filter(|c| !c.is_control()).collect());
    }
    if g.len() > MAX_LINES {
        let d = g.len() - MAX_LINES;
        g.drain(0..d);
    }
}

pub fn tail(n: usize) -> Vec<String> {
    let Ok(g) = LOG.lock() else { return vec![] };
    g[g.len().saturating_sub(n)..].to_vec()
}

pub fn clear() {
    if let Ok(mut g) = LOG.lock() {
        g.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_log_collects_lines_and_tools() {
        let p = std::env::temp_dir().join(format!("vp-fulllog-{}.log", std::process::id()));
        open_file(&p).unwrap();
        set_echo(true);
        push("linia programu".into());
        let mut c = Command::new("sh");
        c.args(["-c", "echo na-stdout; echo na-stderr >&2; exit 3"]);
        let o = c.logged_output().unwrap();
        assert_eq!(o.status.code(), Some(3));
        let cap = Capture::new("narzędzie w tle").unwrap();
        let st = Command::new("sh").args(["-c", "echo z-tla"]).stdout(cap.stdio()).status().unwrap();
        assert!(st.success());
        cap.finish();
        assert_eq!(close_file().as_deref(), Some(p.as_path()));
        let t = std::fs::read_to_string(&p).unwrap();
        for s in ["] linia programu", "$ sh -c \"echo na-stdout", "--- stdout ---\nna-stdout", "na-stderr", "kod wyjścia: 3", "=== narzędzie w tle", "z-tla"] {
            assert!(t.contains(s), "brak {s:?} w:\n{t}");
        }
        set_echo(false);
        let _ = std::fs::remove_file(&p);
    }
}
