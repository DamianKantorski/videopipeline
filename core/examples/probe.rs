//! Podgląd odczytu informacji o pliku: `cargo run -p videopipeline-core --example probe -- plik.mkv`
//! (z `--features libav` — przez biblioteki FFmpeg zamiast ffprobe).
fn main() {
    for a in std::env::args().skip(1) {
        let p = std::path::Path::new(&a);
        println!("== {a}  [{}]", videopipeline_core::probe::backend());
        match videopipeline_core::probe::inspect(p) {
            Ok(i) => {
                println!("duration={:?} tags={:?}", i.duration, sorted(&i.tags));
                for s in &i.streams {
                    println!(
                        "  #{} {} {} ch={} pix={} bits={} range={} space={} trc={} prim={} {}x{} fps={} def={} tags={:?}",
                        s.index, s.kind, s.codec, s.channels, s.pix_fmt, s.bits_per_raw_sample, s.color_range,
                        s.color_space, s.color_transfer, s.color_primaries, s.width, s.height, s.r_frame_rate,
                        s.default, sorted(&s.tags)
                    );
                }
            }
            Err(e) => println!("BŁĄD: {e:#}"),
        }
        match videopipeline_core::probe::probe(p) {
            Ok(m) => println!("media: audio={:?} subs={:?} width={:?} dur={:?} hdr={} full={}", m.audio, m.subs, m.width, m.duration, m.color.is_hdr(), m.color.full_range()),
            Err(e) => println!("probe BŁĄD: {e:#}"),
        }
        println!("fps={:?} size={:?} apple={}", videopipeline_core::probe::video_fps(p), videopipeline_core::probe::video_size(p), videopipeline_core::probe::is_apple_device(p));
    }
}

fn sorted(m: &std::collections::HashMap<String, String>) -> Vec<(&String, &String)> {
    let mut v: Vec<_> = m.iter().collect();
    v.sort();
    v
}
