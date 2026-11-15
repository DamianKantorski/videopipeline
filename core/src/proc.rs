//! Procesy potomne: rejestr PID-ów (do przerwania) i flaga anulowania.
use std::{
    ffi::OsStr,
    process::{Command, Output, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    thread,
    time::Duration,
};

/// Polecenie dla narzędzia pomocniczego (ffmpeg, ffprobe, mpv…).
/// Windows: proces nie dołącza się do konsoli programu (CREATE_NO_WINDOW) — narzędzie konsolowe
/// nie może zmienić trybu konsoli ani zablokować okna z TUI (ConPTY w videopipeline-term, conhost,
/// Windows Terminal). Wejście i wyjście narzędzi i tak są przekierowane (null, potok, plik).
pub fn command(program: impl AsRef<OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

pub static PIDS: Mutex<Vec<u32>> = Mutex::new(Vec::new());
static CANCEL: AtomicBool = AtomicBool::new(false);

pub fn cancelled() -> bool {
    CANCEL.load(Ordering::SeqCst)
}

pub fn set_cancel(v: bool) {
    CANCEL.store(v, Ordering::SeqCst);
}

/// Liczba kodowań działających w naszym procesie (biblioteki FFmpeg w wątkach).
static ENCODING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn encoding_begin() {
    ENCODING.fetch_add(1, Ordering::SeqCst);
}

pub fn encoding_end() {
    // Zawsze w parze z encoding_begin (strażnik w wątku kodowania).
    ENCODING.fetch_sub(1, Ordering::SeqCst);
}

/// Procesy, których obciążenie karty pokazuje monitor: nasz własny w trakcie kodowania i narzędzia pomocnicze.
pub fn monitored_pids() -> Vec<u32> {
    let mut v: Vec<u32> = PIDS.lock().map(|g| g.clone()).unwrap_or_default();
    if ENCODING.load(Ordering::SeqCst) > 0 {
        v.push(std::process::id());
    }
    v
}

pub fn register(pid: u32) {
    if let Ok(mut g) = PIDS.lock() {
        g.push(pid);
    }
}

pub fn unregister(pid: u32) {
    if let Ok(mut g) = PIDS.lock() {
        g.retain(|p| *p != pid);
    }
}

/// Zatrzymuje wszystkie zarejestrowane procesy (TERM, potem KILL).
pub fn kill_all() {
    let pids: Vec<u32> = PIDS.lock().map(|g| g.clone()).unwrap_or_default();
    for force in [false, true] {
        for p in &pids {
            crate::platform::kill_pid(*p, force);
        }
        if !force {
            thread::sleep(Duration::from_millis(500));
        }
    }
}

/// Uruchamia polecenie (stdout/stderr -> null), zwraca kod wyjścia.
/// stdin ustawia wołający.
pub fn run(cmd: &mut Command) -> Option<i32> {
    if crate::log::full() {
        // Pełny log: wyjście narzędzia zbierane i zapisywane razem z wywołaniem.
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let child = cmd.spawn().ok()?;
        let id = child.id();
        register(id);
        let out = child.wait_with_output();
        unregister(id);
        let out = out.ok()?;
        crate::log::tool(cmd, out.status.code(), &out.stdout, &out.stderr);
        return out.status.code();
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let id = child.id();
    register(id);
    let st = child.wait();
    unregister(id);
    st.ok()?.code()
}

/// Uruchamia polecenie i zbiera wyjście (stdin -> null).
pub fn output(cmd: &mut Command) -> Option<Output> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = cmd.spawn().ok()?;
    let id = child.id();
    register(id);
    let out = child.wait_with_output().ok();
    unregister(id);
    if let Some(o) = &out {
        crate::log::tool(cmd, o.status.code(), &o.stdout, &o.stderr);
    }
    out
}
