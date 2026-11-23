//! A minimal MP4 muxer for one H.264 track, in pure Rust.
//!
//! The on-device video server sends Annex-B H.264 (start codes, SPS/PPS in
//! a config packet). This writes the ISO BMFF file QuickTime, browsers and
//! ffmpeg play: `ftyp`, then `moov` (so it starts playing before the whole
//! file is read, "fast start"), then one `mdat` holding every sample as one
//! chunk. The parameter sets go into the `avcC` box and are dropped from the
//! samples, whose NAL units get 4-byte length prefixes. Times come from the
//! encoder's presentation timestamps (microseconds) on a 90 kHz track
//! clock; frames are in decode order with no B-frames (MediaCodec's surface
//! encoders send none at `KEY_LATENCY` 1), so no `ctts` is needed.
//!
//! Why not a crate: `mp4` (0.14) pulls in its own reader/writer stack and
//! has had no release since 2023; younger muxers are unproven. The subset a
//! screen recording needs is ~300 lines and is checked here by parsing the
//! output back.

/// One encoded frame.
#[derive(Debug, Clone)]
pub struct Frame<'a> {
    /// Presentation time in microseconds (any origin).
    pub pts_us: u64,
    pub key: bool,
    /// Annex-B NAL units.
    pub data: &'a [u8],
}

/// Track clock: 90 kHz, the usual video timescale.
const TIMESCALE: u32 = 90_000;
/// Movie header clock (milliseconds).
const MOVIE_TIMESCALE: u32 = 1000;
/// The duration of the last frame (and of frames whose timestamps don't
/// increase), 1/30 s.
const DEFAULT_TICKS: u32 = TIMESCALE / 30;

/// The NAL units of an Annex-B byte stream, without start codes.
pub fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push((i, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    if starts.is_empty() {
        return if data.is_empty() { vec![] } else { vec![data] };
    }
    let mut out = Vec::with_capacity(starts.len());
    for (n, &(_, body)) in starts.iter().enumerate() {
        let mut end = starts.get(n + 1).map_or(data.len(), |&(sc, _)| sc);
        // A 4-byte start code's leading zero, and trailing zero bytes.
        while end > body && data[end - 1] == 0 {
            end -= 1;
        }
        if end > body {
            out.push(&data[body..end]);
        }
    }
    out
}

fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1f)
}

/// SPS and PPS found in `data`.
fn parameter_sets(data: &[u8]) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let mut sps = Vec::new();
    let mut pps = Vec::new();
    for nal in nal_units(data) {
        match nal_type(nal) {
            7 => sps.push(nal.to_vec()),
            8 => pps.push(nal.to_vec()),
            _ => {}
        }
    }
    (sps, pps)
}

/// The `avcC` record (ISO/IEC 14496-15 AVCDecoderConfigurationRecord).
fn avcc(sps: &[Vec<u8>], pps: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    let first = sps.first().ok_or("no SPS in the stream")?;
    if first.len() < 4 {
        return Err("SPS too short".into());
    }
    if pps.is_empty() {
        return Err("no PPS in the stream".into());
    }
    let mut b = vec![
        1,
        first[1],
        first[2],
        first[3],
        0xFF,
        0xE0 | sps.len() as u8,
    ];
    for s in sps {
        b.extend_from_slice(&(s.len() as u16).to_be_bytes());
        b.extend_from_slice(s);
    }
    b.push(pps.len() as u8);
    for p in pps {
        b.extend_from_slice(&(p.len() as u16).to_be_bytes());
        b.extend_from_slice(p);
    }
    Ok(b)
}

/// A frame's NAL units as an MP4 sample: 4-byte big-endian lengths, the
/// parameter sets and access unit delimiters left out.
fn sample(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 16);
    for nal in nal_units(data) {
        // SPS, PPS, access unit delimiter.
        if (7..=9).contains(&nal_type(nal)) {
            continue;
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

fn mp4_box(kind: &[u8; 4], parts: &[&[u8]]) -> Vec<u8> {
    let len: usize = 8 + parts.iter().map(|p| p.len()).sum::<usize>();
    let mut b = Vec::with_capacity(len);
    b.extend_from_slice(&(len as u32).to_be_bytes());
    b.extend_from_slice(kind);
    for p in parts {
        b.extend_from_slice(p);
    }
    b
}

/// A full box: version 0 and `flags`.
fn full_box(kind: &[u8; 4], flags: u32, parts: &[&[u8]]) -> Vec<u8> {
    let vf = flags.to_be_bytes();
    let mut all: Vec<&[u8]> = vec![&vf];
    all.extend_from_slice(parts);
    mp4_box(kind, &all)
}

fn u32s(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

/// The unity transformation matrix of `mvhd` and `tkhd`.
fn matrix() -> Vec<u8> {
    u32s(&[0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000])
}

/// Mux `frames` (decode order, the first one a key frame) with the
/// parameter sets in `config` (or in the frames themselves) into an MP4
/// file of `width` x `height`.
pub fn mux_h264(
    config: &[u8],
    width: u32,
    height: u32,
    frames: &[Frame<'_>],
) -> Result<Vec<u8>, String> {
    let first_key = frames
        .iter()
        .position(|f| f.key)
        .ok_or("no key frame to start the video at")?;
    let frames = &frames[first_key..];
    let (mut sps, mut pps) = parameter_sets(config);
    if sps.is_empty() || pps.is_empty() {
        // Some encoders repeat the parameter sets in each key frame.
        let (s, p) = parameter_sets(frames[0].data);
        sps = s;
        pps = p;
    }
    let avcc = avcc(&sps, &pps)?;

    // Samples and their durations on the 90 kHz clock.
    let samples: Vec<Vec<u8>> = frames.iter().map(|f| sample(f.data)).collect();
    let mut durations = Vec::with_capacity(frames.len());
    for w in frames.windows(2) {
        let us = w[1].pts_us.saturating_sub(w[0].pts_us);
        let ticks = (us * u64::from(TIMESCALE) / 1_000_000).min(u64::from(u32::MAX)) as u32;
        durations.push(if ticks == 0 { DEFAULT_TICKS } else { ticks });
    }
    durations.push(DEFAULT_TICKS);
    let total_ticks: u64 = durations.iter().map(|&d| u64::from(d)).sum();
    let movie_duration = (total_ticks * u64::from(MOVIE_TIMESCALE) / u64::from(TIMESCALE)) as u32;

    let ftyp = mp4_box(
        b"ftyp",
        &[b"isom", &0x200u32.to_be_bytes(), b"isomiso2avc1mp41"],
    );
    let mdat_payload: usize = samples.iter().map(Vec::len).sum();
    let build_moov = |chunk_offset: u32| -> Vec<u8> {
        let mvhd = full_box(
            b"mvhd",
            0,
            &[
                &u32s(&[0, 0, MOVIE_TIMESCALE, movie_duration, 0x0001_0000]),
                &0x0100u16.to_be_bytes(),
                &[0u8; 10],
                &matrix(),
                &[0u8; 24],
                &2u32.to_be_bytes(),
            ],
        );
        let tkhd = full_box(
            b"tkhd",
            3, // enabled, in movie
            &[
                &u32s(&[0, 0, 1, 0, movie_duration]),
                &[0u8; 8],
                &[0u8; 8], // layer, alternate group, volume, reserved
                &matrix(),
                &u32s(&[width << 16, height << 16]),
            ],
        );
        let mdhd = full_box(
            b"mdhd",
            0,
            &[
                &u32s(&[0, 0, TIMESCALE, total_ticks.min(u64::from(u32::MAX)) as u32]),
                &0x55c4u16.to_be_bytes(), // "und"
                &[0u8; 2],
            ],
        );
        let hdlr = full_box(
            b"hdlr",
            0,
            &[&[0u8; 4], b"vide", &[0u8; 12], b"Velocity video\0"],
        );
        let vmhd = full_box(b"vmhd", 1, &[&[0u8; 8]]);
        let url = full_box(b"url ", 1, &[]);
        let dref = full_box(b"dref", 0, &[&1u32.to_be_bytes(), &url]);
        let dinf = mp4_box(b"dinf", &[&dref]);
        let avcc_box = mp4_box(b"avcC", &[&avcc]);
        let mut compressor = [0u8; 32];
        let name = b"Velocity H.264";
        compressor[0] = name.len() as u8;
        compressor[1..=name.len()].copy_from_slice(name);
        let avc1 = mp4_box(
            b"avc1",
            &[
                &[0u8; 6],
                &1u16.to_be_bytes(), // data reference index
                &[0u8; 16],
                &(width as u16).to_be_bytes(),
                &(height as u16).to_be_bytes(),
                &u32s(&[0x0048_0000, 0x0048_0000, 0]),
                &1u16.to_be_bytes(), // frame count
                &compressor,
                &0x0018u16.to_be_bytes(),
                &(-1i16).to_be_bytes(),
                &avcc_box,
            ],
        );
        let stsd = full_box(b"stsd", 0, &[&1u32.to_be_bytes(), &avc1]);
        // Durations, run-length coded.
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for &d in &durations {
            match runs.last_mut() {
                Some((n, v)) if *v == d => *n += 1,
                _ => runs.push((1, d)),
            }
        }
        let mut stts_body = (runs.len() as u32).to_be_bytes().to_vec();
        for (n, d) in &runs {
            stts_body.extend(u32s(&[*n, *d]));
        }
        let stts = full_box(b"stts", 0, &[&stts_body]);
        let keys: Vec<u32> = frames
            .iter()
            .enumerate()
            .filter(|(_, f)| f.key)
            .map(|(i, _)| i as u32 + 1)
            .collect();
        let mut stss_body = (keys.len() as u32).to_be_bytes().to_vec();
        stss_body.extend(u32s(&keys));
        let stss = full_box(b"stss", 0, &[&stss_body]);
        let mut stsz_body = u32s(&[0, samples.len() as u32]);
        stsz_body.extend(samples.iter().flat_map(|s| (s.len() as u32).to_be_bytes()));
        let stsz = full_box(b"stsz", 0, &[&stsz_body]);
        let stsc = full_box(b"stsc", 0, &[&u32s(&[1, 1, samples.len() as u32, 1])]);
        let stco = full_box(b"stco", 0, &[&u32s(&[1, chunk_offset])]);
        let stbl = mp4_box(b"stbl", &[&stsd, &stts, &stss, &stsz, &stsc, &stco]);
        let minf = mp4_box(b"minf", &[&vmhd, &dinf, &stbl]);
        let mdia = mp4_box(b"mdia", &[&mdhd, &hdlr, &minf]);
        let trak = mp4_box(b"trak", &[&tkhd, &mdia]);
        mp4_box(b"moov", &[&mvhd, &trak])
    };
    // The moov's size doesn't depend on the offset's value: build it once
    // to learn where the samples start, then for real.
    let moov_len = build_moov(0).len();
    let offset = ftyp.len() + moov_len + 8;
    if offset + mdat_payload > u32::MAX as usize {
        return Err("video too large for a 32-bit MP4".into());
    }
    let moov = build_moov(offset as u32);
    let mut out = Vec::with_capacity(offset + mdat_payload);
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&moov);
    out.extend_from_slice(&((8 + mdat_payload) as u32).to_be_bytes());
    out.extend_from_slice(b"mdat");
    for s in &samples {
        out.extend_from_slice(s);
    }
    Ok(out)
}

/// What [`parse`] reads back out of a file this module wrote (tests, and
/// a sanity check before a file is kept).
#[derive(Debug, Default, PartialEq)]
pub struct Parsed {
    pub width: u32,
    pub height: u32,
    pub timescale: u32,
    pub sample_sizes: Vec<u32>,
    pub durations: Vec<u32>,
    pub key_frames: Vec<u32>,
    pub chunk_offset: u32,
    pub avcc: Vec<u8>,
    /// Top-level boxes in order.
    pub top: Vec<String>,
}

fn rd_u32(b: &[u8], at: usize) -> Result<u32, String> {
    b.get(at..at + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| format!("truncated at {at}"))
}

/// A visitor of boxes: `(kind, payload)`.
type Visit<'f, 'a> = dyn FnMut(&str, &'a [u8]) -> Result<(), String> + 'f;

/// Walk the boxes of `b` (a box's payload), calling `f(kind, payload)`.
fn walk<'a>(b: &'a [u8], f: &mut Visit<'_, 'a>) -> Result<(), String> {
    let mut at = 0;
    while at < b.len() {
        let len = rd_u32(b, at)? as usize;
        let kind = b.get(at + 4..at + 8).ok_or("truncated box header")?;
        if len < 8 || at + len > b.len() {
            return Err(format!("bad box length {len} at {at}"));
        }
        f(&String::from_utf8_lossy(kind), &b[at + 8..at + len])?;
        at += len;
    }
    Ok(())
}

/// Parse the boxes [`mux_h264`] writes and check they nest and add up.
pub fn parse(file: &[u8]) -> Result<Parsed, String> {
    let mut p = Parsed::default();
    let mut moov: Option<&[u8]> = None;
    walk(file, &mut |k, body| {
        p.top.push(k.to_string());
        if k == "moov" {
            moov = Some(body);
        }
        Ok(())
    })?;
    let moov = moov.ok_or("no moov")?;
    fn find<'a>(b: &'a [u8], path: &[&str]) -> Result<&'a [u8], String> {
        let mut found = None;
        walk(b, &mut |k, body| {
            if k == path[0] && found.is_none() {
                found = Some(body);
            }
            Ok(())
        })?;
        let body = found.ok_or_else(|| format!("no {}", path[0]))?;
        if path.len() == 1 {
            Ok(body)
        } else {
            find(body, &path[1..])
        }
    }
    let tkhd = find(moov, &["trak", "tkhd"])?;
    p.width = rd_u32(tkhd, 76)? >> 16;
    p.height = rd_u32(tkhd, 80)? >> 16;
    let mdhd = find(moov, &["trak", "mdia", "mdhd"])?;
    p.timescale = rd_u32(mdhd, 12)?;
    let stbl = find(moov, &["trak", "mdia", "minf", "stbl"])?;
    let stsd = find(stbl, &["stsd"])?;
    // stsd: version/flags, count, then the avc1 sample entry (78 bytes of
    // fields before its child boxes).
    let avc1 = stsd.get(8 + 8..).ok_or("short stsd")?;
    p.avcc = find(avc1.get(78..).ok_or("short avc1")?, &["avcC"])?.to_vec();
    let stts = find(stbl, &["stts"])?;
    for i in 0..rd_u32(stts, 4)? as usize {
        let (n, d) = (rd_u32(stts, 8 + 8 * i)?, rd_u32(stts, 12 + 8 * i)?);
        p.durations.extend(std::iter::repeat_n(d, n as usize));
    }
    let stss = find(stbl, &["stss"])?;
    for i in 0..rd_u32(stss, 4)? as usize {
        p.key_frames.push(rd_u32(stss, 8 + 4 * i)?);
    }
    let stsz = find(stbl, &["stsz"])?;
    for i in 0..rd_u32(stsz, 8)? as usize {
        p.sample_sizes.push(rd_u32(stsz, 12 + 4 * i)?);
    }
    let stco = find(stbl, &["stco"])?;
    p.chunk_offset = rd_u32(stco, 8)?;
    let total: u64 = p.sample_sizes.iter().map(|&s| u64::from(s)).sum();
    if u64::from(p.chunk_offset) + total > file.len() as u64 {
        return Err("samples run past the end of the file".into());
    }
    if p.durations.len() != p.sample_sizes.len() {
        return Err("stts and stsz disagree".into());
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0x67, 0x42, 0xc0, 0x29, 0x8d, 0x68, 0x0a];
    const PPS: &[u8] = &[0x68, 0xce, 0x01, 0xa8];

    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        for n in nals {
            v.extend_from_slice(&[0, 0, 0, 1]);
            v.extend_from_slice(n);
        }
        v
    }

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let mut data = vec![0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3];
        data.extend_from_slice(&[0, 0, 0, 1, 0x65, 0, 9]);
        let nals = nal_units(&data);
        assert_eq!(nals, vec![&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 0, 9]]);
        assert!(nal_units(&[]).is_empty());
    }

    #[test]
    fn round_trip_parses_its_own_output() {
        let config = annexb(&[SPS, PPS]);
        let idr = annexb(&[&[0x65, 0x88, 0x84, 0x00, 0x33]]);
        let p1 = annexb(&[&[0x41, 0x9a, 0x02]]);
        let p2 = annexb(&[&[0x09, 0x10], &[0x41, 0x9a, 0x03, 0x04]]); // with an AUD
        let idr2 = annexb(&[SPS, PPS, &[0x65, 0x88, 0x01]]); // inline parameter sets
        let frames = [
            Frame {
                pts_us: 1_000_000,
                key: true,
                data: &idr,
            },
            Frame {
                pts_us: 1_033_333,
                key: false,
                data: &p1,
            },
            Frame {
                pts_us: 1_100_000,
                key: false,
                data: &p2,
            },
            Frame {
                pts_us: 2_100_000,
                key: true,
                data: &idr2,
            },
        ];
        let file = mux_h264(&config, 584, 1280, &frames).unwrap();
        let p = parse(&file).unwrap();
        assert_eq!(
            p.top,
            vec!["ftyp", "moov", "mdat"],
            "fast start: moov first"
        );
        assert_eq!((p.width, p.height, p.timescale), (584, 1280, 90_000));
        assert_eq!(p.key_frames, vec![1, 4]);
        // 33.333 ms, 66.667 ms, 1 s, then the default 1/30 s.
        assert_eq!(p.durations, vec![2999, 6000, 90_000, 3000]);
        // Lengths replace start codes; SPS/PPS/AUD are dropped from samples.
        assert_eq!(p.sample_sizes, vec![4 + 5, 4 + 3, 4 + 4, 4 + 3]);
        let first = &file[p.chunk_offset as usize..][..9];
        assert_eq!(first, &[0, 0, 0, 5, 0x65, 0x88, 0x84, 0x00, 0x33]);
        // avcC: version, profile, compat, level, 4-byte lengths, 1 SPS, 1 PPS.
        assert_eq!(&p.avcc[..6], &[1, 0x42, 0xc0, 0x29, 0xff, 0xe1]);
        assert_eq!(&p.avcc[8..8 + SPS.len()], SPS);
        // The file ends exactly at the last sample.
        let total: u32 = p.sample_sizes.iter().sum();
        assert_eq!(p.chunk_offset as usize + total as usize, file.len());
    }

    #[test]
    fn starts_at_the_first_key_frame() {
        let config = annexb(&[SPS, PPS]);
        let p = annexb(&[&[0x41, 1]]);
        let i = annexb(&[&[0x65, 1]]);
        let frames = [
            Frame {
                pts_us: 0,
                key: false,
                data: &p,
            },
            Frame {
                pts_us: 10,
                key: true,
                data: &i,
            },
        ];
        let parsed = parse(&mux_h264(&config, 8, 8, &frames).unwrap()).unwrap();
        assert_eq!(parsed.sample_sizes.len(), 1);
        assert!(mux_h264(&config, 8, 8, &frames[..1]).is_err());
    }

    #[test]
    fn needs_parameter_sets() {
        let i = annexb(&[&[0x65, 1]]);
        let frames = [Frame {
            pts_us: 0,
            key: true,
            data: &i,
        }];
        assert!(mux_h264(&[], 8, 8, &frames).unwrap_err().contains("SPS"));
    }

    #[test]
    fn rejects_corrupt_files() {
        assert!(parse(b"\0\0\0\x04ftyp").is_err());
        assert!(parse(b"\0\0\0\x08free").is_err(), "no moov");
    }
}
