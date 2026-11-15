//! Odczyt metadanych dynamicznego HDR ze strumienia HEVC źródła: RPU Dolby Vision (NAL 62)
//! i HDR10+ (SEI ST 2094-40) — w programie, zamiast `dovi_tool extract-rpu` i `hdr10plus_tool extract`.
//!
//! Wynik w tych samych formatach co z tych narzędzi (plik RPU jak dovi_tool, bajt w bajt; HDR10+:
//! JSON z SceneInfo), w kolejności wyświetlania klatek — numeracja POC jak przy wstrzykiwaniu
//! (inject.rs), więc metadane trafiają z powrotem do tych samych klatek.
use crate::inject::{self, PocParser, Reorder};
use anyhow::{Context, Result};
use std::{
    collections::{BTreeMap, HashMap},
    io::Write,
    path::Path,
};

#[derive(Default)]
struct Frame {
    rpu: Option<Vec<u8>>,
    hdr10plus: Option<Vec<u8>>,
}

/// Zbiera metadane z kolejnych NAL (w kolejności dekodowania) i układa je w kolejności wyświetlania.
pub struct Extractor {
    want_rpu: bool,
    want_hdr10plus: bool,
    pp: PocParser,
    reorder: Reorder,
    next_id: u64,
    /// Bieżąca (otwarta) klatka: numer dekodowania i zebrane metadane.
    cur: Option<(u64, Frame)>,
    /// Metadane przed pierwszym slice'em klatki (SEI HDR10+ stoi przed obrazem).
    pending: Frame,
    closed: HashMap<u64, Frame>,
    pres_of: HashMap<u64, u64>,
    rpus: BTreeMap<u64, Vec<u8>>,
    hdr10plus: BTreeMap<u64, Vec<u8>>,
    frames: u64,
}

/// Wynik odczytu: metadane w kolejności wyświetlania.
pub struct Collected {
    pub frames: u64,
    pub rpus: Vec<Vec<u8>>,
    pub hdr10plus: Vec<Vec<u8>>,
}

impl Extractor {
    pub fn new(want_rpu: bool, want_hdr10plus: bool) -> Self {
        Extractor {
            want_rpu,
            want_hdr10plus,
            pp: PocParser::new(),
            reorder: Reorder::new(),
            next_id: 0,
            cur: None,
            pending: Frame::default(),
            closed: HashMap::new(),
            pres_of: HashMap::new(),
            rpus: BTreeMap::new(),
            hdr10plus: BTreeMap::new(),
            frames: 0,
        }
    }

    pub fn nal(&mut self, nal: &[u8]) -> Result<()> {
        if nal.len() < 3 || nal[0] & 0x80 != 0 {
            return Ok(());
        }
        if ((nal[0] & 1) << 5) | (nal[1] >> 3) != 0 {
            return Ok(()); // warstwy dodatkowe (nuh_layer_id > 0)
        }
        let t = inject::nal_type(nal);
        if inject::is_vcl(t) {
            if nal[2] & 0x80 != 0 {
                // pierwszy slice nowej klatki
                self.close();
                let (poc, new_seq) = self.pp.slice_poc(nal)?;
                let id = self.next_id;
                self.next_id += 1;
                self.reorder.add(id, poc, new_seq);
                self.cur = Some((id, std::mem::take(&mut self.pending)));
                self.settle();
            }
            return Ok(());
        }
        if inject::starts_au(t) {
            self.close();
        }
        self.pp.param(t, nal)?;
        match t {
            inject::NAL_UNSPEC62 if self.want_rpu => match &mut self.cur {
                Some((_, f)) => f.rpu = Some(nal.to_vec()),
                None => self.pending.rpu = Some(nal.to_vec()),
            },
            39 if self.want_hdr10plus => {
                if let Some(p) = hdr10plus_payload(nal) {
                    self.pending.hdr10plus = Some(p);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn close(&mut self) {
        if let Some((id, f)) = self.cur.take() {
            self.closed.insert(id, f);
            self.frames += 1;
        }
        self.settle();
    }

    /// Klatki zamknięte i z numerem wyświetlania → metadane na swoje miejsce.
    fn settle(&mut self) {
        for (id, pres) in std::mem::take(&mut self.reorder.assigned) {
            self.pres_of.insert(id, pres);
        }
        let ready: Vec<u64> = self.closed.keys().filter(|id| self.pres_of.contains_key(id)).copied().collect();
        for id in ready {
            let f = self.closed.remove(&id).unwrap();
            let pres = self.pres_of.remove(&id).unwrap();
            if let Some(r) = f.rpu {
                self.rpus.insert(pres, r);
            }
            if let Some(h) = f.hdr10plus {
                self.hdr10plus.insert(pres, h);
            }
        }
    }

    pub fn finish(mut self) -> Collected {
        self.close();
        self.reorder.flush();
        self.settle();
        Collected {
            frames: self.frames,
            rpus: self.rpus.into_values().collect(),
            hdr10plus: self.hdr10plus.into_values().collect(),
        }
    }
}

/// Dane ST 2094-40 (od kodu kraju T.35) z SEI prefiksowego, jeśli to HDR10+.
fn hdr10plus_payload(nal: &[u8]) -> Option<Vec<u8>> {
    let d = inject::unescape(nal, usize::MAX);
    let mut i = 2;
    let num = |i: &mut usize| -> Option<usize> {
        let mut v = 0usize;
        loop {
            let b = *d.get(*i)?;
            *i += 1;
            v += b as usize;
            if b != 0xFF {
                return Some(v);
            }
        }
    };
    while i + 2 <= d.len() {
        if d[i] == 0x80 && i + 1 == d.len() {
            break; // rbsp_trailing_bits
        }
        let ty = num(&mut i)?;
        let size = num(&mut i)?;
        let pl = d.get(i..i + size)?;
        // user_data_registered_itu_t_t35: USA (0xB5), Samsung (0x003C), 0x0001, aplikacja 4
        if ty == 4 && pl.len() > 6 && pl[0] == 0xB5 && pl[1..3] == [0x00, 0x3C] && pl[3..5] == [0x00, 0x01] && pl[5] == 4 {
            return Some(pl.to_vec());
        }
        i += size;
    }
    None
}

/// Zapis wyników: RPU jak `dovi_tool extract-rpu`, HDR10+ jak `hdr10plus_tool extract`.
/// Zwraca (liczba RPU, liczba klatek HDR10+); pliki powstają tylko przy niezerowej liczbie.
pub fn save(c: &Collected, rpu_out: Option<&Path>, hdr10plus_out: Option<&Path>) -> Result<(usize, usize)> {
    let mut n_rpu = 0;
    if let Some(path) = rpu_out.filter(|_| !c.rpus.is_empty()) {
        let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
        for r in &c.rpus {
            // Tylko RPU, które da się odczytać (uszkodzone pomija też dovi_tool).
            if dolby_vision::rpu::dovi_rpu::DoviRpu::parse_unspec62_nalu(r).is_err() {
                continue;
            }
            // Jak dovi_tool: kod startowy i dane RPU bez 2-bajtowego nagłówka NAL (0x7C01).
            f.write_all(&[0, 0, 0, 1])?;
            f.write_all(&r[2..])?;
            n_rpu += 1;
        }
        f.flush()?;
        if n_rpu == 0 {
            drop(f);
            let _ = std::fs::remove_file(path);
        }
    }
    let mut n_h10 = 0;
    if let Some(path) = hdr10plus_out.filter(|_| !c.hdr10plus.is_empty()) {
        let list: Vec<hdr10plus::metadata::Hdr10PlusMetadata> =
            c.hdr10plus.iter().filter_map(|p| hdr10plus::metadata::Hdr10PlusMetadata::parse(p).ok()).collect();
        if !list.is_empty() {
            let refs: Vec<&_> = list.iter().collect();
            let json = hdr10plus::metadata_json::generate_json(&refs, "videopipeline", env!("CARGO_PKG_VERSION"));
            std::fs::write(path, serde_json::to_string_pretty(&json)?).with_context(|| format!("zapis {}", path.display()))?;
            n_h10 = list.len();
        }
    }
    Ok((n_rpu, n_h10))
}

/// Odczyt całego pliku bez okna postępu (narzędzia, testy): RPU i HDR10+ w kolejności wyświetlania.
pub fn from_file(input: &Path) -> Result<Collected> {
    let mut ex = Extractor::new(true, true);
    #[cfg(feature = "libav")]
    crate::libav::hevc_nals(input, |n| ex.nal(n), |_| true)?;
    #[cfg(not(feature = "libav"))]
    {
        let mut ff = crate::proc::command("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(input)
            .args(["-map", "0:v:0", "-c:v", "copy", "-bsf:v", "hevc_mp4toannexb", "-f", "hevc", "-"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .context("nie można uruchomić ffmpeg")?;
        let mut out = ff.stdout.take().context("brak stdout ffmpeg")?;
        inject::split_annexb(&mut out, |n| ex.nal(&n))?;
        let _ = ff.wait();
    }
    Ok(ex.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hdr10plus_sei_is_recognised() {
        // SEI zbudowane przez bibliotekę hdr10plus z przykładowych metadanych wraca jako te same dane.
        let meta = hdr10plus::metadata::Hdr10PlusMetadata {
            itu_t_t35_country_code: 0xB5,
            itu_t_t35_terminal_provider_code: 0x003C,
            itu_t_t35_terminal_provider_oriented_code: 0x0001,
            application_identifier: 4,
            application_version: 1,
            num_windows: 1,
            targeted_system_display_maximum_luminance: 400,
            maxscl: [17830, 16895, 14252],
            average_maxrgb: 1291,
            num_distribution_maxrgb_percentiles: 0,
            distribution_maxrgb: vec![],
            fraction_bright_pixels: 1,
            ..Default::default()
        };
        let Ok(nal) = hdr10plus::hevc::encode_hdr10plus_nal(&meta, false) else {
            return;
        };
        let p = hdr10plus_payload(&nal).expect("SEI HDR10+");
        let back = hdr10plus::metadata::Hdr10PlusMetadata::parse(&p).unwrap();
        assert_eq!(back.maxscl, meta.maxscl);
        assert_eq!(back.average_maxrgb, 1291);
        assert!(hdr10plus_payload(&[0x4E, 0x01, 0x05, 0x02, 0xAA, 0xBB, 0x80]).is_none());
    }
}
