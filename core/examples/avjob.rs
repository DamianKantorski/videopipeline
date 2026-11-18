//! Uruchamia zadanie silnika jak polecenie ffmpeg: `cargo run -p videopipeline-core --example avjob -- <argumenty>`.
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let (ok, err) = videopipeline_core::av::run_args(&a);
    println!("{} {}", if ok { "OK" } else { "BŁĄD" }, err);
}
