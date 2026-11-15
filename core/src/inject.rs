//! Dolby Vision i HDR10+ w locie: strumień HEVC z kodera (Annex-B na stdout ffmpeg) → RPU (NAL 62)
//! i SEI HDR10+ doklejane do każdej klatki → Matroska na stdin drugiego ffmpeg, który składa plik
//! wynikowy z audio i napisami. Bez plików pośrednich wideo.
//!
//! Przypisanie metadanych do klatek jak w dovi_tool `inject-rpu` / hdr10plus_tool `inject`:
//! metadane są w kolejności wyświetlania (POC), klatki przychodzą w kolejności dekodowania.
//! RPU idzie na koniec klatki, SEI HDR10+ tuż przed pierwszy slice.
//!
//! Kolejność wyświetlania liczymy strumieniowo: bufor klatek czekających na numer (jak bufor
//! wyjściowy dekodera, „bumping”). Strumień zgodny z HEVC ma najwyżej 15 klatek przestawionych
//! (sps_max_num_reorder_pics ≤ 15), więc przy buforze 16 klatka o najmniejszym POC ma już pewny numer.
//! Granica sekwencji (IDR / BLA / CRA rozpoczynający) zeruje POC — wtedy opróżniamy bufor.
//!
//! Strumień do drugiego ffmpeg idzie w Matroska, bo surowe HEVC z B-klatkami nie ma znaczników
//! czasu (ffmpeg nie umie ich odtworzyć), a MKV niesie też rekord konfiguracji Dolby Vision (dvcC/dvvC).
//! Bloki i CodecPrivate są w Annex-B — ffmpeg przy zapisie sam zamienia je na hvcC / długości NAL.
use anyhow::{bail, Context, Result};
use dolby_vision::rpu::{dovi_rpu::DoviRpu, ConversionMode};
use hdr10plus::metadata_json::{Hdr10PlusJsonMetadata, MetadataJsonRoot};
use std::{
    collections::VecDeque,
    path::Path,
};

// ---------------------------------------------------------------------------------------------
// Metadane wejściowe
// ---------------------------------------------------------------------------------------------

pub struct Metadata {
    pub rpus: Vec<DoviRpu>,
    pub hdr10plus: Vec<Hdr10PlusJsonMetadata>,
    /// Profil DV zapisany w rekordzie konfiguracji (po ewentualnej konwersji 7 → 8.1).
    pub dv_profile: u8,
    /// RPU profilu 7 (dwie warstwy) przerobione na 8.1 — kodujemy tylko warstwę bazową.
    pub converted_p7: bool,
}

/// Przesunięcia obszaru aktywnego (L5, czarne pasy) po zmianie rozdzielczości obrazu.
/// Zwraca liczbę RPU z L5 innym niż zero.
pub fn scale_l5(rpus: &mut [DoviRpu], (sx, sy): (f64, f64)) -> Result<usize> {
    use dolby_vision::rpu::extension_metadata::blocks::ExtMetadataBlock;
    let mut n = 0;
    for r in rpus.iter_mut() {
        let Some(ExtMetadataBlock::Level5(b)) = r.vdr_dm_data.as_ref().and_then(|d| d.get_block(5)).cloned() else {
            continue;
        };
        let (l, rt, t, bo) = (
            b.active_area_left_offset,
            b.active_area_right_offset,
            b.active_area_top_offset,
            b.active_area_bottom_offset,
        );
        if l == 0 && rt == 0 && t == 0 && bo == 0 {
            continue;
        }
        let s = |v: u16, f: f64| (v as f64 * f).round() as u16;
        r.set_active_area_offsets(s(l, sx), s(rt, sx), s(t, sy), s(bo, sy))?;
        n += 1;
    }
    Ok(n)
}

impl Metadata {
    /// `l5_scale`: współczynniki skalowania obrazu (szerokość, wysokość) — przelicza obszar aktywny L5.
    pub fn load(rpu: Option<&Path>, hdr10plus_json: Option<&Path>, l5_scale: Option<(f64, f64)>) -> Result<Self> {
        let mut rpus = match rpu {
            Some(p) => dolby_vision::rpu::utils::parse_rpu_file(p)
                .with_context(|| format!("odczyt RPU {}", p.display()))?,
            None => vec![],
        };
        let mut converted_p7 = false;
        if rpus.first().is_some_and(|r| r.dovi_profile == 7) {
            for r in rpus.iter_mut() {
                r.convert_with_mode(ConversionMode::To81).context("konwersja RPU profilu 7 → 8.1")?;
            }
            converted_p7 = true;
        }
        if let Some(f) = l5_scale {
            scale_l5(&mut rpus, f)?;
        }
        let dv_profile = rpus.first().map(|r| r.dovi_profile).unwrap_or(0);
        let hdr10plus = match hdr10plus_json {
            Some(p) => {
                let root = MetadataJsonRoot::from_file(p).with_context(|| format!("odczyt HDR10+ {}", p.display()))?;
                if root.scene_info.is_empty() {
                    bail!("pusty plik HDR10+ (brak SceneInfo)");
                }
                root.scene_info
            }
            None => vec![],
        };
        Ok(Metadata { rpus, hdr10plus, dv_profile, converted_p7 })
    }
}

#[derive(Debug, Default)]
pub struct Stats {
    pub frames: u64,
    pub rpus: usize,
    pub hdr10plus: usize,
    pub width: u32,
    pub height: u32,
    pub dv_level: u8,
    /// RPU profilu 7 przerobione na 8.1.
    pub converted_p7: bool,
}

impl Stats {
    /// Ostrzeżenia jak w dovi_tool / hdr10plus_tool przy innej liczbie klatek niż metadanych.
    pub fn warnings(&self) -> Vec<String> {
        let mut w = vec![];
        for (what, n) in [("RPU Dolby Vision", self.rpus), ("HDR10+", self.hdr10plus)] {
            if n > 0 && n as u64 != self.frames {
                w.push(format!(
                    "{what}: {n} wpisów, wideo {} klatek — {}",
                    self.frames,
                    if (n as u64) < self.frames { "ostatni powielony do końca" } else { "nadmiarowe pominięte" }
                ));
            }
        }
        w
    }
}

// ---------------------------------------------------------------------------------------------
// Bity: SPS / PPS / nagłówek slice'a (tylko pola potrzebne do POC i wymiarów)
// ---------------------------------------------------------------------------------------------

/// Usuwa bajty 0x03 zapobiegające emulacji kodu startowego (00 00 03 → 00 00).
pub(crate) fn unescape(data: &[u8], limit: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len().min(limit));
    let mut zeros = 0;
    for &b in data.iter().take(limit) {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(d: &'a [u8]) -> Self {
        Bits { d, pos: 0 }
    }
    fn bit(&mut self) -> Result<bool> {
        let byte = *self.d.get(self.pos / 8).context("koniec danych NAL")?;
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Ok(b == 1)
    }
    fn bits(&mut self, n: u32) -> Result<u64> {
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | self.bit()? as u64;
        }
        Ok(v)
    }
    fn skip(&mut self, n: usize) {
        self.pos += n;
    }
    fn ue(&mut self) -> Result<u64> {
        let mut zeros = 0;
        while !self.bit()? {
            zeros += 1;
            if zeros > 32 {
                bail!("niepoprawny kod Exp-Golomb");
            }
        }
        Ok((1u64 << zeros) - 1 + self.bits(zeros)?)
    }
}

#[derive(Clone, Default)]
struct Sps {
    log2_max_poc_lsb: u32,
    separate_colour_plane: bool,
    width: u32,
    height: u32,
}

#[derive(Clone, Default)]
struct Pps {
    sps_id: usize,
    output_flag_present: bool,
    num_extra_slice_header_bits: usize,
}

/// `nal` z nagłówkiem (2 bajty), bez kodu startowego.
fn parse_sps(nal: &[u8]) -> Result<(usize, Sps)> {
    let d = unescape(nal, 1024);
    let mut b = Bits::new(&d[2..]);
    b.skip(4); // sps_video_parameter_set_id
    let max_sub_layers_minus1 = b.bits(3)? as usize;
    b.skip(1); // sps_temporal_id_nesting_flag
    // profile_tier_level(1, max_sub_layers_minus1)
    b.skip(96); // general_profile_space … general_level_idc (88 + 8 bitów)
    let mut sub_profile = vec![false; max_sub_layers_minus1];
    let mut sub_level = vec![false; max_sub_layers_minus1];
    for i in 0..max_sub_layers_minus1 {
        sub_profile[i] = b.bit()?;
        sub_level[i] = b.bit()?;
    }
    if max_sub_layers_minus1 > 0 {
        b.skip(2 * (8 - max_sub_layers_minus1));
    }
    for i in 0..max_sub_layers_minus1 {
        if sub_profile[i] {
            b.skip(88);
        }
        if sub_level[i] {
            b.skip(8);
        }
    }
    let id = b.ue()? as usize;
    let chroma = b.ue()?;
    let separate_colour_plane = chroma == 3 && b.bit()?;
    let mut width = b.ue()? as u32;
    let mut height = b.ue()? as u32;
    if b.bit()? {
        // conformance_window: przesunięcia w jednostkach chrominancji
        let (sw, sh) = match chroma {
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        let (l, r, t, bo) = (b.ue()?, b.ue()?, b.ue()?, b.ue()?);
        width = width.saturating_sub((sw * (l + r)) as u32);
        height = height.saturating_sub((sh * (t + bo)) as u32);
    }
    b.ue()?; // bit_depth_luma_minus8
    b.ue()?; // bit_depth_chroma_minus8
    let log2_max_poc_lsb = b.ue()? as u32 + 4;
    if log2_max_poc_lsb > 16 {
        bail!("niepoprawny SPS (log2_max_pic_order_cnt_lsb {log2_max_poc_lsb})");
    }
    Ok((id, Sps { log2_max_poc_lsb, separate_colour_plane, width, height }))
}

fn parse_pps(nal: &[u8]) -> Result<(usize, Pps)> {
    let d = unescape(nal, 256);
    let mut b = Bits::new(&d[2..]);
    let id = b.ue()? as usize;
    let sps_id = b.ue()? as usize;
    b.skip(1); // dependent_slice_segments_enabled_flag
    let output_flag_present = b.bit()?;
    let num_extra_slice_header_bits = b.bits(3)? as usize;
    Ok((id, Pps { sps_id, output_flag_present, num_extra_slice_header_bits }))
}

pub(crate) fn nal_type(nal: &[u8]) -> u8 {
    (nal[0] >> 1) & 0x3f
}
pub(crate) fn is_vcl(t: u8) -> bool {
    t < 32
}
pub(crate) fn is_irap(t: u8) -> bool {
    (16..=23).contains(&t)
}
fn is_idr(t: u8) -> bool {
    t == 19 || t == 20
}
fn is_bla(t: u8) -> bool {
    (16..=18).contains(&t)
}
/// NAL zaczynający nową klatkę, jeśli bieżąca ma już slice.
pub(crate) fn starts_au(t: u8) -> bool {
    matches!(t, 32..=35 | 39 | 41..=44 | 48..=55)
}

const NAL_EOS: u8 = 36;
pub(crate) const NAL_UNSPEC62: u8 = 62;

// ---------------------------------------------------------------------------------------------
// Kolejność wyświetlania
// ---------------------------------------------------------------------------------------------

/// Bufor wyjściowy: klatki w kolejności dekodowania czekają na swój numer w kolejności wyświetlania.
pub(crate) struct Reorder {
    pending: Vec<(i64, u64)>, // (POC, numer dekodowania)
    next_pres: u64,
    pub(crate) assigned: Vec<(u64, u64)>, // (numer dekodowania, numer wyświetlania) — do odebrania
}

/// Zapas większy od największego dopuszczalnego przestawienia klatek w HEVC (15).
const REORDER_DEPTH: usize = 16;

impl Reorder {
    pub(crate) fn new() -> Self {
        Reorder { pending: vec![], next_pres: 0, assigned: vec![] }
    }
    fn bump(&mut self) {
        let (i, _) = self.pending.iter().enumerate().min_by_key(|(_, (poc, _))| *poc).unwrap();
        let (_, id) = self.pending.swap_remove(i);
        self.assigned.push((id, self.next_pres));
        self.next_pres += 1;
    }
    pub(crate) fn flush(&mut self) {
        while !self.pending.is_empty() {
            self.bump();
        }
    }
    pub(crate) fn add(&mut self, id: u64, poc: i64, new_sequence: bool) {
        if new_sequence {
            self.flush();
        }
        self.pending.push((poc, id));
        while self.pending.len() > REORDER_DEPTH {
            self.bump();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Matroska (minimalny zapis strumieniowy)
// ---------------------------------------------------------------------------------------------

/// Poziom DV z liczby pikseli na sekundę i szerokości (tabela jak w FFmpeg / mkvmerge).
pub(crate) fn dovi_level(width: u32, height: u32, fps: (u64, u64)) -> u8 {
    const LEVELS: [(u64, u32); 13] = [
        (1280 * 720 * 24, 1280),
        (1280 * 720 * 30, 1280),
        (1920 * 1080 * 24, 1920),
        (1920 * 1080 * 30, 2560),
        (1920 * 1080 * 60, 3840),
        (3840 * 2160 * 24, 3840),
        (3840 * 2160 * 30, 3840),
        (3840 * 2160 * 48, 3840),
        (3840 * 2160 * 60, 3840),
        (3840 * 2160 * 120, 3840),
        (3840 * 2160 * 120, 7680),
        (7680 * 4320 * 60, 7680),
        (7680 * 4320 * 120, 7680),
    ];
    let pps = width as u64 * height as u64 * fps.0 / fps.1.max(1);
    LEVELS
        .iter()
        .position(|&(max_pps, max_w)| pps <= max_pps && width <= max_w)
        .map(|i| i as u8 + 1)
        .unwrap_or(LEVELS.len() as u8)
}

struct Au {
    nals: Vec<Vec<u8>>,
    has_vcl: bool,
    key: bool,
}

impl Au {
    fn new() -> Self {
        Au { nals: vec![], has_vcl: false, key: false }
    }
}

struct Queued {
    id: u64,
    pres: Option<u64>,
    au: Au,
}

fn set<T: Clone>(v: &mut Vec<Option<T>>, id: usize, x: T) {
    if v.len() <= id {
        v.resize(id + 1, None);
    }
    v[id] = Some(x);
}

/// Numeracja klatek HEVC: zestawy parametrów (SPS/PPS) i POC z nagłówka pierwszego slice'a klatki.
pub(crate) struct PocParser {
    sps: Vec<Option<Sps>>,
    pps: Vec<Option<Pps>>,
    prev_tid0_poc: i64,
    first_pic: bool,
    after_eos: bool,
    /// Wymiary obrazu z pierwszego SPS użytego przez klatkę.
    pub width: u32,
    pub height: u32,
}

impl PocParser {
    pub(crate) fn new() -> Self {
        PocParser { sps: vec![], pps: vec![], prev_tid0_poc: 0, first_pic: true, after_eos: false, width: 0, height: 0 }
    }

    /// SPS, PPS i koniec sekwencji (EOS) — reszta NAL bez znaczenia dla numeracji.
    pub(crate) fn param(&mut self, t: u8, nal: &[u8]) -> Result<()> {
        match t {
            33 => {
                let (id, s) = parse_sps(nal).context("SPS")?;
                set(&mut self.sps, id, s);
            }
            34 => {
                let (id, p) = parse_pps(nal).context("PPS")?;
                set(&mut self.pps, id, p);
            }
            NAL_EOS => self.after_eos = true,
            _ => {}
        }
        Ok(())
    }

    /// POC i wymiary z pierwszego slice'a klatki. Zwraca (POC, początek nowej sekwencji).
    pub(crate) fn slice_poc(&mut self, nal: &[u8]) -> Result<(i64, bool)> {
        let t = nal_type(nal);
        let temporal_id = (nal[1] & 7).saturating_sub(1);
        let d = unescape(nal, 64);
        let mut b = Bits::new(&d[2..]);
        b.skip(1); // first_slice_segment_in_pic_flag (= 1)
        if is_irap(t) {
            b.skip(1); // no_output_of_prior_pics_flag
        }
        let pps_id = b.ue()? as usize;
        let pps = self.pps.get(pps_id).cloned().flatten().with_context(|| format!("brak PPS {pps_id}"))?;
        let sps = self.sps.get(pps.sps_id).cloned().flatten().with_context(|| format!("brak SPS {}", pps.sps_id))?;
        if self.width == 0 {
            self.width = sps.width;
            self.height = sps.height;
        }
        b.skip(pps.num_extra_slice_header_bits);
        b.ue()?; // slice_type
        if pps.output_flag_present {
            b.skip(1);
        }
        if sps.separate_colour_plane {
            b.skip(2);
        }
        let no_rasl_output = is_idr(t) || is_bla(t) || (t == 21 && (self.first_pic || self.after_eos));
        let poc = if is_idr(t) {
            0
        } else {
            let lsb = b.bits(sps.log2_max_poc_lsb)? as i64;
            let max = 1i64 << sps.log2_max_poc_lsb;
            let msb = if is_irap(t) && no_rasl_output {
                0
            } else {
                let prev_lsb = self.prev_tid0_poc.rem_euclid(max);
                let prev_msb = self.prev_tid0_poc - prev_lsb;
                if lsb < prev_lsb && prev_lsb - lsb >= max / 2 {
                    prev_msb + max
                } else if lsb > prev_lsb && lsb - prev_lsb > max / 2 {
                    prev_msb - max
                } else {
                    prev_msb
                }
            };
            msb + lsb
        };
        // prevTid0Pic: TemporalId 0, nie RASL/RADL, nie nieodniesiony podwarstwowy (typy parzyste ≤ 14).
        let sub_layer_non_ref = t <= 14 && t.is_multiple_of(2);
        if temporal_id == 0 && !(6..=9).contains(&t) && !sub_layer_non_ref {
            self.prev_tid0_poc = poc;
        }
        let new_sequence = is_irap(t) && no_rasl_output && !self.first_pic;
        self.first_pic = false;
        self.after_eos = false;
        Ok((poc, new_sequence))
    }

}

/// NAL z bufora Annex-B w pamięci (np. pakiet z kontenera MPEG-TS).
pub(crate) fn annexb_nals(buf: &[u8]) -> Vec<&[u8]> {
    let mut out = vec![];
    let mut start: Option<usize> = None;
    let mut i = 2;
    while i < buf.len() {
        if buf[i] == 1 && buf[i - 1] == 0 && buf[i - 2] == 0 {
            if let Some(s) = start {
                let mut end = i - 2;
                while end > s && buf[end - 1] == 0 {
                    end -= 1;
                }
                out.push(&buf[s..end]);
            }
            start = Some(i + 1);
        }
        i += 1;
    }
    if let Some(s) = start {
        let mut end = buf.len();
        while end > s && buf[end - 1] == 0 {
            end -= 1;
        }
        if end > s {
            out.push(&buf[s..end]);
        }
    }
    out
}

/// Dolby Vision RPU i SEI HDR10+ doklejane do pakietów kodera HEVC (jeden pakiet = jedna klatka,
/// Annex-B). Klatki dostają metadane według kolejności wyświetlania (POC), a pakiety wychodzą w tej
/// samej kolejności, w jakiej weszły — z opóźnieniem potrzebnym na ustalenie kolejności wyświetlania.
pub struct PacketInjector {
    meta: Metadata,
    pp: PocParser,
    reorder: Reorder,
    queue: VecDeque<Queued>,
    next_id: u64,
    /// NAL bez obrazu z pakietu bez klatki — dołączane do następnej klatki.
    carry: Vec<Vec<u8>>,
    pub stats: Stats,
}

impl PacketInjector {
    pub fn new(meta: Metadata) -> Self {
        let stats = Stats { rpus: meta.rpus.len(), hdr10plus: meta.hdr10plus.len(), converted_p7: meta.converted_p7, ..Default::default() };
        PacketInjector { meta, pp: PocParser::new(), reorder: Reorder::new(), queue: VecDeque::new(), next_id: 0, carry: vec![], stats }
    }

    /// (profil, zgodność warstwy bazowej) do rekordu konfiguracji DV — gdy są RPU.
    /// Wynik kodera ma znaczniki PQ: zgodność 1 (HDR10); profil 5 nie ma zgodnej warstwy bazowej.
    pub fn dovi(&self) -> Option<(u8, u8)> {
        (!self.meta.rpus.is_empty()).then_some((self.meta.dv_profile, if self.meta.dv_profile == 5 { 0 } else { 1 }))
    }

    /// Zestawy parametrów z nagłówka kodera (VPS/SPS/PPS, Annex-B albo hvcC).
    pub fn headers(&mut self, extradata: &[u8]) -> Result<()> {
        if extradata.len() >= 23 && extradata[0] == 1 {
            let mut i = 23;
            for _ in 0..extradata[22] {
                let Some(h) = extradata.get(i..i + 3) else { break };
                let count = u16::from_be_bytes([h[1], h[2]]) as usize;
                i += 3;
                for _ in 0..count {
                    let Some(l) = extradata.get(i..i + 2) else { break };
                    let l = u16::from_be_bytes([l[0], l[1]]) as usize;
                    if let Some(n) = extradata.get(i + 2..i + 2 + l) {
                        if n.len() >= 2 {
                            self.pp.param(nal_type(n), n)?;
                        }
                    }
                    i += 2 + l;
                }
            }
            return Ok(());
        }
        for n in annexb_nals(extradata) {
            if n.len() >= 2 {
                self.pp.param(nal_type(n), n)?;
            }
        }
        Ok(())
    }

    /// Pakiet kodera → pakiety gotowe do zapisu (z metadanymi), w kolejności wejścia.
    pub fn push(&mut self, packet: &[u8]) -> Result<Vec<Vec<u8>>> {
        let mut au = Au::new();
        au.nals = std::mem::take(&mut self.carry);
        let mut slot: Option<u64> = None;
        for nal in annexb_nals(packet) {
            if nal.len() < 2 || nal[0] & 0x80 != 0 {
                continue;
            }
            let t = nal_type(nal);
            if ((nal[0] & 1) << 5) | (nal[1] >> 3) != 0 {
                au.nals.push(nal.to_vec()); // warstwy dodatkowe — bez analizy
                continue;
            }
            if is_vcl(t) {
                if nal.get(2).is_some_and(|b| b & 0x80 != 0) && slot.is_none() {
                    let (poc, new_seq) = self.pp.slice_poc(nal)?;
                    let id = self.next_id;
                    self.next_id += 1;
                    self.reorder.add(id, poc, new_seq);
                    slot = Some(id);
                    au.key = is_irap(t);
                    au.has_vcl = true;
                }
                au.nals.push(nal.to_vec());
                continue;
            }
            self.pp.param(t, nal)?;
            // Istniejące RPU zastępujemy nowymi.
            if t == NAL_UNSPEC62 && !self.meta.rpus.is_empty() {
                continue;
            }
            au.nals.push(nal.to_vec());
        }
        match slot {
            Some(id) => self.queue.push_back(Queued { id, pres: None, au }),
            None => {
                self.carry = au.nals;
                bail!("pakiet kodera bez klatki obrazu");
            }
        }
        self.drain(false)
    }

    /// Koniec strumienia: pozostałe pakiety.
    pub fn flush(&mut self) -> Result<Vec<Vec<u8>>> {
        let out = self.drain(true)?;
        if !self.queue.is_empty() {
            bail!("{} klatek bez numeru wyświetlania na końcu strumienia", self.queue.len());
        }
        Ok(out)
    }

    fn drain(&mut self, all: bool) -> Result<Vec<Vec<u8>>> {
        if all {
            self.reorder.flush();
        }
        for (id, pres) in std::mem::take(&mut self.reorder.assigned) {
            let front = self.queue.front().map(|q| q.id).unwrap_or(0);
            if let Some(q) = id.checked_sub(front).and_then(|i| self.queue.get_mut(i as usize)) {
                q.pres = Some(pres);
            }
        }
        let mut out = vec![];
        while self.queue.front().is_some_and(|q| q.pres.is_some()) {
            let q = self.queue.pop_front().unwrap();
            out.push(self.payload(q.pres.unwrap(), q.au)?);
        }
        Ok(out)
    }

    fn payload(&mut self, pres: u64, au: Au) -> Result<Vec<u8>> {
        let meta = &self.meta;
        let pick = |n: usize| (n > 0).then(|| (pres as usize).min(n - 1));
        let mut nals = au.nals;
        if let Some(i) = pick(meta.hdr10plus.len()) {
            let sei = hdr10plus::hevc::encode_hevc_from_json(&meta.hdr10plus[i], true)
                .with_context(|| format!("SEI HDR10+ dla klatki {pres}"))?;
            let at = nals.iter().position(|n| is_vcl(nal_type(n))).unwrap_or(0);
            nals.insert(at, sei);
        }
        if let Some(i) = pick(meta.rpus.len()) {
            let rpu = meta.rpus[i].write_hevc_unspec62_nalu().with_context(|| format!("RPU dla klatki {pres}"))?;
            nals.push(rpu); // RPU zawsze ostatnie w klatce (jak dovi_tool — tego oczekuje FFmpeg)
        }
        let mut payload = Vec::with_capacity(nals.iter().map(|n| n.len() + 4).sum());
        for n in &nals {
            payload.extend_from_slice(&[0, 0, 0, 1]);
            payload.extend_from_slice(n);
        }
        self.stats.frames += 1;
        Ok(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reorder_matches_poc_order() {
        // Hierarchiczne B-klatki: kolejność dekodowania z POC 0 8 4 2 1 3 6 5 7 | IDR 0 4 2 1 3
        let pocs: &[(i64, bool)] = &[
            (0, false), (8, false), (4, false), (2, false), (1, false), (3, false), (6, false), (5, false),
            (7, false), (0, true), (4, false), (2, false), (1, false), (3, false),
        ];
        let mut r = Reorder::new();
        for (id, &(poc, seq)) in pocs.iter().enumerate() {
            r.add(id as u64, poc, seq);
        }
        r.flush();
        let mut pres = vec![0u64; pocs.len()];
        for (id, p) in r.assigned {
            pres[id as usize] = p;
        }
        assert_eq!(pres, [0, 8, 4, 2, 1, 3, 6, 5, 7, 9, 13, 11, 10, 12]);
    }

    #[test]
    fn dovi_record_and_level() {
        assert_eq!(dovi_level(3840, 2160, (24000, 1001)), 6);
        assert_eq!(dovi_level(1920, 1080, (24, 1)), 3);
        assert_eq!(dovi_level(1280, 720, (24000, 1001)), 1);
    }

    #[test]
    fn l5_follows_scaling() {
        use dolby_vision::rpu::extension_metadata::blocks::level5::ExtMetadataBlockLevel5;
        use dolby_vision::rpu::generate::GenerateConfig;
        let cfg = GenerateConfig {
            length: 1,
            level5: ExtMetadataBlockLevel5::from_offsets(0, 0, 280, 282),
            ..Default::default()
        };
        let mut rpus = vec![DoviRpu::profile81_config(&cfg).unwrap()];
        assert_eq!(scale_l5(&mut rpus, (0.5, 0.5)).unwrap(), 1);
        let enc = rpus[0].write_hevc_unspec62_nalu().unwrap();
        let back = DoviRpu::parse_unspec62_nalu(&enc).unwrap();
        let l5 = back.vdr_dm_data.as_ref().and_then(|d| d.get_block(5)).cloned();
        match l5 {
            Some(dolby_vision::rpu::extension_metadata::blocks::ExtMetadataBlock::Level5(b)) => {
                assert_eq!((b.active_area_top_offset, b.active_area_bottom_offset), (140, 141));
            }
            _ => panic!("brak L5"),
        }
    }

}
