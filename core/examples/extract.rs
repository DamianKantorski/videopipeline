//! Odczyt RPU Dolby Vision i HDR10+ z pliku (jak w analizie):
//! `cargo run -p videopipeline-core --example extract -- plik.mkv rpu.bin hdr10plus.json`
fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let (input, rpu, json) = (std::path::Path::new(&a[0]), std::path::Path::new(&a[1]), std::path::Path::new(&a[2]));
    let c = videopipeline_core::extract::from_file(input)?;
    let (r, h) = videopipeline_core::extract::save(&c, Some(rpu), Some(json))?;
    println!("klatek {} · RPU {r} · HDR10+ {h}", c.frames);
    Ok(())
}
