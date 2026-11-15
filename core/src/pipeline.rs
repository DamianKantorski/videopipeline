//! Wątek roboczy: pre-flight -> analiza -> kodowanie GPU (NVENC/QSV/VAAPI) -> mux (logika z V9).
use crate::{
    analysis::{self, Analysis},
    config::*,
    copy, encode,
    model::{Panel, State},
    mux, preflight, proc,
};
use anyhow::Result;
use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
};

#[derive(Clone, Debug, PartialEq)]
pub enum RunStatus {
    Running,
    Done,
    Failed(String),
    Cancelled,
}

pub type SharedStatus = Arc<Mutex<RunStatus>>;

pub fn spawn(opts: Opts, files: Vec<PathBuf>, panel: Panel, status: SharedStatus) -> JoinHandle<()> {
    thread::spawn(move || {
        let result = run(&opts, &files, &panel);
        let fin = if proc::cancelled() {
            RunStatus::Cancelled
        } else {
            match result {
                Ok(()) => RunStatus::Done,
                Err(e) => RunStatus::Failed(format!("{e:#}")),
            }
        };
        if let Ok(mut g) = status.lock() {
            *g = fin;
        }
    })
}

fn run(opts: &Opts, files: &[PathBuf], panel: &Panel) -> Result<()> {
    for f in files {
        if let Some(n) = f.file_name() {
            panel.register(&n.to_string_lossy());
        }
    }
    if opts.full_log {
        let _ = fs::create_dir_all(&opts.output_dir);
        let path = opts
            .output_dir
            .join(format!("videopipeline_{}.log", chrono::Local::now().format("%Y-%m-%d_%H-%M-%S")));
        match crate::log::open_file(&path) {
            Ok(()) => {
                plog!("Pełny log: {}", path.display());
                crate::log::file_line(&format!(
                    "VideoPipeline {} · {} {} · {}",
                    env!("CARGO_PKG_VERSION"),
                    std::env::consts::OS,
                    std::env::consts::ARCH,
                    std::env::args().collect::<Vec<_>>().join(" ")
                ));
                crate::log::file_line(&format!("Ustawienia: {opts:?}"));
                for f in files {
                    crate::log::file_line(&format!("Plik: {}", f.display()));
                }
            }
            Err(e) => plog!("UWAGA: nie można utworzyć pełnego logu {}: {e}", path.display()),
        }
    }
    let res = run_inner(opts, files, panel);
    if let Err(e) = &res {
        plog!("BŁĄD: {e:#}");
    }
    if proc::cancelled() {
        plog!("Przerwano.");
    }
    if let Some(p) = crate::log::close_file() {
        plog!("Pełny log zapisany: {}", p.display());
    }
    res
}

fn run_inner(opts: &Opts, files: &[PathBuf], panel: &Panel) -> Result<()> {
    panel.draw("PRE-FLIGHT");
    plog!("Pre-flight: sprawdzam FFmpeg / {}...", opts.engine.label());
    let rt = preflight::run(opts)?;
    let res = stages(opts, &rt, files, panel);
    let _ = fs::remove_dir_all(&rt.progress_dir);
    res
}

fn stages(opts: &Opts, rt: &Runtime, files: &[PathBuf], panel: &Panel) -> Result<()> {
    let mut panel = panel.clone();
    fs::create_dir_all(&opts.output_dir)?;
    plog!("Katalog wyjściowy: {}", opts.output_dir.display());

    // ===== ETAP 1: analiza (sekwencyjnie) =====================================
    plog!("ETAP 1/3: ANALIZA PLIKÓW");
    for input in files {
        if proc::cancelled() {
            return Ok(());
        }
        let filename = input.file_name().unwrap().to_string_lossy().to_string();
        let p = opts.paths_for(input);
        panel.set(&filename, State::Analysis, "Analiza wejścia");

        if p.output.exists() {
            // Gotowy wynik: ewentualnie tylko dokończ kopię (-c).
            if opts.copy {
                match copy::copy_final_to_source_encoded(input, &p.output) {
                    Ok(()) => panel.set(&filename, State::Done, "Kopia w encoded zweryfikowana SHA-512"),
                    Err(e) => {
                        plog!("    BŁĄD kopiowania: {e:#}");
                        panel.set(&filename, State::Error, "Kopiowanie do encoded nie powiodło się");
                    }
                }
            } else {
                panel.set(&filename, State::Done, "Plik wynikowy już istnieje");
            }
            continue;
        }
        if p.analysis.exists() {
            // Analiza jest nieaktualna, gdy pominęła etap, którego opcje teraz wymagają
            // (np. wcześniej z --no-dv, teraz z DV).
            let stale = Analysis::load(&p.analysis).map(|a| a.stale_for(opts)).unwrap_or(true);
            if !stale {
                panel.set(&filename, State::Nvenc, "Analiza pominięta — istnieje plik analizy");
                continue;
            }
            plog!(" -> [{filename}] analiza nieaktualna względem opcji — wykonuję ponownie");
            let _ = fs::remove_file(&p.analysis);
        }
        plog!(" -> Analiza pliku: {filename}");
        match analysis::analyze(opts, input, &mut panel).and_then(|a| a.save(&p.analysis)) {
            Ok(()) => panel.set(&filename, State::Nvenc, "Analiza zakończona"),
            Err(e) => {
                plog!("    BŁĄD analizy [{filename}]: {e:#}");
                panel.set(&filename, State::Error, "Błąd analizy");
            }
        }
    }

    // ===== ETAP 2: kodowanie GPU ================================================
    plog!("ETAP 2/3: KODOWANIE WIDEO (DUAL {})", opts.engine.label());
    // Tylko pliki z bieżącej kolejki (katalog wyjściowy może zawierać stare analizy).
    let loaded: Vec<(PathBuf, Analysis)> = opts
        .list_analysis_files()
        .into_iter()
        .filter_map(|f| Analysis::load(&f).ok().map(|a| (f, a)))
        .filter(|(_, a)| files.contains(&a.input_file))
        .collect();

    if loaded.is_empty() {
        plog!("Brak plików do zakodowania.");
    } else {
        for (_, a) in &loaded {
            panel.register(&a.filename);
        }
        let list: Vec<Analysis> = loaded.iter().map(|(_, a)| a.clone()).collect();
        encode::run_stage(opts, rt, &list, &panel);
    }
    if proc::cancelled() {
        return Ok(());
    }

    // ===== ETAP 3: zakończenie ==================================================
    plog!("ETAP 3/3: ZAKOŃCZENIE (sprzątanie, kopia -c)");
    panel.draw("ETAP 3/3 — ZAKOŃCZENIE");
    for (file, a) in &loaded {
        if proc::cancelled() {
            return Ok(());
        }
        if let Err(e) = mux::run(opts, a, file, &mut panel) {
            plog!(" -> [{}] BŁĄD: {e:#}", a.filename);
            panel.set(&a.filename, State::Error, format!("{e}"));
        }
    }

    panel.draw("PODSUMOWANIE");
    plog!("ZAKOŃCZONO. Pliki w: {}", opts.output_dir.display());
    Ok(())
}
