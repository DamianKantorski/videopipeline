//! Zadanie silnika z wiersza poleceń (opcje w składni FFmpeg) — do testów i porównań:
//! `cargo run -p videopipeline-core --example avrun -- -i in.mkv -map 0:v:0 -c:v libx265 -f matroska out.mkv`
//! Zmienne RPU / HDR10PLUS: pliki metadanych do wstrzyknięcia w locie.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let job = match videopipeline_core::av::Job::parse(&args) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("BŁĄD: {e:#}");
            std::process::exit(2);
        }
    };
    let (rpu, json) = (std::env::var("RPU").ok(), std::env::var("HDR10PLUS").ok());
    let inject = (rpu.is_some() || json.is_some()).then(|| videopipeline_core::av::InjectCfg {
        meta: videopipeline_core::inject::Metadata::load(rpu.as_deref().map(std::path::Path::new), json.as_deref().map(std::path::Path::new), None)
            .expect("metadane"),
    });
    match videopipeline_core::av::run(&job, videopipeline_core::av::Hooks { inject, stop: None }) {
        Ok(r) => {
            if std::env::var("VERBOSE").is_ok() {
                for l in &r.log {
                    eprintln!("{l}");
                }
            }
            println!("OK: {} klatek {:?} {:?}", r.frames, r.inject, r.frame_meta);
        }
        Err(e) => {
            eprintln!("BŁĄD: {e:#}");
            std::process::exit(1);
        }
    }
}
