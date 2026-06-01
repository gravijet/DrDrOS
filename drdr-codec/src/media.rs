//! A from-scratch **media container probe** for MP4/MOV (ISO-BMFF boxes)
//! and Matroska/WebM (EBML).
//!
//! Decoding modern video (H.264/H.265/VP9/AV1) or audio (AAC/Opus) from
//! scratch is an enormous undertaking and out of scope — so instead of
//! pretending, this reads the **container metadata** the way a "Get Info"
//! / properties panel does: duration, pixel dimensions, the codec
//! four-cc / codec id, and the track list. That is genuinely useful (you
//! can see what a file *is*) and completely honest about what we do and
//! don't decode.

/// What a probe could learn about a media file. Empty / `None` fields
/// just mean "not found in the parts we parse".
#[derive(Debug, Default, Clone)]
pub struct MediaInfo {
    pub container: String,
    pub duration_secs: f64,
    pub width: u32,
    pub height: u32,
    pub video_codec: String,
    pub audio_codec: String,
    pub track_count: u32,
}

impl MediaInfo {
    /// Human-readable lines for the info viewer.
    pub fn summary(&self) -> Vec<String> {
        let mut out = vec![format!("Container : {}", self.container)];
        if self.duration_secs > 0.0 {
            let total = self.duration_secs.round() as u64;
            out.push(format!(
                "Duration  : {}:{:02} ({:.2}s)",
                total / 60,
                total % 60,
                self.duration_secs
            ));
        }
        if self.width > 0 && self.height > 0 {
            out.push(format!("Resolution: {} x {}", self.width, self.height));
        }
        if !self.video_codec.is_empty() {
            out.push(format!("Video     : {}", self.video_codec));
        }
        if !self.audio_codec.is_empty() {
            out.push(format!("Audio     : {}", self.audio_codec));
        }
        if self.track_count > 0 {
            out.push(format!("Tracks    : {}", self.track_count));
        }
        out.push(String::new());
        out.push("(container metadata only — DrDrOS does not".into());
        out.push(" decode compressed video/audio frames)".into());
        out
    }
}

/// Probe a media file. `ext` (lower-case, no dot) is a hint; the magic
/// bytes are the real decider.
pub fn probe_media(data: &[u8], ext: &str) -> MediaInfo {
    if is_mp4(data) {
        return probe_mp4(data);
    }
    if data.len() >= 4 && data[0..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        return probe_matroska(data, ext);
    }
    // Fall back to an ext-labelled stub so the viewer still says something.
    MediaInfo {
        container: match ext {
            "mp4" | "m4v" | "mov" => "MP4 / QuickTime".into(),
            "mkv" | "webm" => "Matroska / WebM".into(),
            "avi" => "AVI (RIFF)".into(),
            "mp3" => "MP3 audio".into(),
            "flac" => "FLAC audio".into(),
            "wav" => "WAV (RIFF) audio".into(),
            "ogg" => "Ogg".into(),
            other => format!("media ({other})"),
        },
        ..Default::default()
    }
}

fn is_mp4(data: &[u8]) -> bool {
    data.len() >= 12 && &data[4..8] == b"ftyp"
}

// ─── MP4 / ISO base media file format ────────────────────────────────

fn be32(d: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}
fn be64(d: &[u8], o: usize) -> u64 {
    let mut v = 0u64;
    for i in 0..8 {
        v = (v << 8) | d[o + i] as u64;
    }
    v
}

fn probe_mp4(data: &[u8]) -> MediaInfo {
    let mut info = MediaInfo { container: "MP4 / ISO-BMFF".into(), ..Default::default() };
    if data.len() >= 12 {
        let brand = String::from_utf8_lossy(&data[8..12]).trim().to_string();
        if !brand.is_empty() {
            info.container = format!("MP4 (brand {brand})");
        }
    }
    walk_mp4(data, 0, data.len(), &mut info);
    info
}

/// Walk a box list in `[start, end)`, descending into containers we care
/// about and harvesting fields into `info`.
fn walk_mp4(data: &[u8], start: usize, end: usize, info: &mut MediaInfo) {
    let mut pos = start;
    while pos + 8 <= end {
        let mut size = be32(data, pos) as usize;
        let kind = &data[pos + 4..pos + 8];
        let mut header = 8usize;
        if size == 1 {
            if pos + 16 > end {
                break;
            }
            size = be64(data, pos + 8) as usize;
            header = 16;
        } else if size == 0 {
            size = end - pos;
        }
        if size < header || pos + size > end {
            break;
        }
        let body = pos + header;
        let body_end = pos + size;
        match kind {
            b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" => {
                walk_mp4(data, body, body_end, info)
            }
            b"mvhd" => parse_mvhd(data, body, body_end, info),
            b"tkhd" => parse_tkhd(data, body, body_end, info),
            b"hdlr" => parse_hdlr(data, body, body_end, info),
            b"stsd" => parse_stsd(data, body, body_end, info),
            _ => {}
        }
        pos += size;
    }
}

fn parse_mvhd(data: &[u8], body: usize, end: usize, info: &mut MediaInfo) {
    if body >= end {
        return;
    }
    let version = data[body];
    let (timescale, duration) = if version == 1 {
        if body + 28 > end {
            return;
        }
        (be32(data, body + 20) as f64, be64(data, body + 24) as f64)
    } else {
        if body + 20 > end {
            return;
        }
        (be32(data, body + 12) as f64, be32(data, body + 16) as f64)
    };
    if timescale > 0.0 {
        info.duration_secs = duration / timescale;
    }
}

fn parse_tkhd(data: &[u8], body: usize, end: usize, info: &mut MediaInfo) {
    // Width/height are the last two 16.16 fixed-point fields of tkhd.
    if end < body + 8 || end < 8 {
        return;
    }
    let w = be32(data, end - 8) >> 16;
    let h = be32(data, end - 4) >> 16;
    if w > 0 && h > 0 {
        info.width = w;
        info.height = h;
    }
}

fn parse_hdlr(data: &[u8], body: usize, end: usize, info: &mut MediaInfo) {
    // handler_type is 4 bytes at offset 8 of the box body.
    if body + 12 > end {
        return;
    }
    let handler = &data[body + 8..body + 12];
    if handler == b"vide" || handler == b"soun" {
        info.track_count += 1;
    }
}

fn parse_stsd(data: &[u8], body: usize, end: usize, info: &mut MediaInfo) {
    // FullBox(4) + entry_count(4), then sample entries: size(4)+format(4).
    if body + 16 > end {
        return;
    }
    let fmt = String::from_utf8_lossy(&data[body + 12..body + 16]).trim().to_string();
    let kind = codec_name(&fmt);
    if is_video_fourcc(&fmt) {
        info.video_codec = kind;
    } else if is_audio_fourcc(&fmt) {
        info.audio_codec = kind;
    } else if info.video_codec.is_empty() {
        info.video_codec = kind;
    }
}

fn is_video_fourcc(f: &str) -> bool {
    matches!(f, "avc1" | "avc3" | "hev1" | "hvc1" | "mp4v" | "vp08" | "vp09" | "av01")
}
fn is_audio_fourcc(f: &str) -> bool {
    matches!(f, "mp4a" | "ac-3" | "ec-3" | "opus" | "Opus" | ".mp3" | "alac")
}

fn codec_name(f: &str) -> String {
    match f {
        "avc1" | "avc3" => "H.264 / AVC".into(),
        "hev1" | "hvc1" => "H.265 / HEVC".into(),
        "vp08" => "VP8".into(),
        "vp09" => "VP9".into(),
        "av01" => "AV1".into(),
        "mp4v" => "MPEG-4 Visual".into(),
        "mp4a" => "AAC".into(),
        "ac-3" => "Dolby AC-3".into(),
        "ec-3" => "Dolby E-AC-3".into(),
        "opus" | "Opus" => "Opus".into(),
        ".mp3" => "MP3".into(),
        "alac" => "ALAC".into(),
        other => other.to_string(),
    }
}

// ─── Matroska / WebM (EBML) ──────────────────────────────────────────

/// Read an EBML variable-length integer at `pos`. Returns `(value,
/// length_in_bytes)`. If `keep_marker` is true the length-descriptor bit
/// is retained (needed to compare element IDs); otherwise it is stripped
/// (element sizes).
fn read_vint(data: &[u8], pos: usize, keep_marker: bool) -> Option<(u64, usize)> {
    let first = *data.get(pos)?;
    if first == 0 {
        return None;
    }
    let len = first.leading_zeros() as usize + 1; // 1..=8
    if pos + len > data.len() {
        return None;
    }
    let mut value = if keep_marker {
        first as u64
    } else {
        (first & (0xFF >> len)) as u64
    };
    for i in 1..len {
        value = (value << 8) | data[pos + i] as u64;
    }
    Some((value, len))
}

fn probe_matroska(data: &[u8], ext: &str) -> MediaInfo {
    let mut info = MediaInfo {
        container: if ext == "webm" { "WebM (Matroska)".into() } else { "Matroska".into() },
        ..Default::default()
    };
    let mut timecode_scale = 1_000_000.0f64; // ns per tick, default
    let mut duration_ticks = 0.0f64;
    walk_ebml(data, 0, data.len(), &mut info, &mut timecode_scale, &mut duration_ticks, 0);
    if duration_ticks > 0.0 {
        info.duration_secs = duration_ticks * timecode_scale / 1_000_000_000.0;
    }
    info
}

#[allow(clippy::too_many_arguments)]
fn walk_ebml(
    data: &[u8],
    start: usize,
    end: usize,
    info: &mut MediaInfo,
    timecode_scale: &mut f64,
    duration_ticks: &mut f64,
    depth: u32,
) {
    if depth > 8 {
        return;
    }
    let mut pos = start;
    while pos < end {
        let Some((id, idlen)) = read_vint(data, pos, true) else { break };
        let Some((size, szlen)) = read_vint(data, pos + idlen, false) else { break };
        let body = pos + idlen + szlen;
        let body_end = (body + size as usize).min(end);
        if body > end {
            break;
        }
        match id {
            // Container elements we descend into.
            0x18538067 | 0x1549A966 | 0x1654AE6B | 0xAE | 0xE0 => {
                if id == 0x1654AE6B || id == 0xAE {
                    // Tracks / a TrackEntry — count entries at the TrackEntry level.
                    if id == 0xAE {
                        info.track_count += 1;
                    }
                }
                walk_ebml(data, body, body_end, info, timecode_scale, duration_ticks, depth + 1);
            }
            0x2AD7B1 => *timecode_scale = read_uint(data, body, body_end) as f64,
            0x4489 => *duration_ticks = read_float(data, body, body_end),
            0x86 => {
                // CodecID string, e.g. "V_MPEG4/ISO/AVC", "A_OPUS".
                let s = String::from_utf8_lossy(&data[body..body_end]).trim().to_string();
                assign_codec(info, &s);
            }
            0xB0 => info.width = read_uint(data, body, body_end) as u32,
            0xBA => info.height = read_uint(data, body, body_end) as u32,
            _ => {}
        }
        // Always advances by at least idlen+szlen (>=2), so no zero-size
        // element can stall the walk.
        pos = body + size as usize;
    }
}

fn read_uint(data: &[u8], start: usize, end: usize) -> u64 {
    let mut v = 0u64;
    for &b in &data[start..end.min(data.len())] {
        v = (v << 8) | b as u64;
    }
    v
}

fn read_float(data: &[u8], start: usize, end: usize) -> f64 {
    match end - start {
        4 if start + 4 <= data.len() => {
            f32::from_be_bytes([data[start], data[start + 1], data[start + 2], data[start + 3]])
                as f64
        }
        8 if start + 8 <= data.len() => {
            let mut b = [0u8; 8];
            b.copy_from_slice(&data[start..start + 8]);
            f64::from_be_bytes(b)
        }
        _ => read_uint(data, start, end) as f64,
    }
}

fn assign_codec(info: &mut MediaInfo, codec_id: &str) {
    let name = match codec_id {
        "V_MPEG4/ISO/AVC" => "H.264 / AVC",
        "V_MPEGH/ISO/HEVC" => "H.265 / HEVC",
        "V_VP8" => "VP8",
        "V_VP9" => "VP9",
        "V_AV1" => "AV1",
        "A_OPUS" => "Opus",
        "A_VORBIS" => "Vorbis",
        "A_AAC" => "AAC",
        "A_FLAC" => "FLAC",
        "A_MPEG/L3" => "MP3",
        other => other,
    };
    if codec_id.starts_with("V_") {
        info.video_codec = name.to_string();
    } else if codec_id.starts_with("A_") {
        info.audio_codec = name.to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built minimal MP4: `ftyp` + `moov`>`mvhd` (v0, timescale
    /// 600, duration 1200 ⇒ 2.0 s). Conforms to ISO-BMFF box framing.
    fn minimal_mp4() -> Vec<u8> {
        let mut mvhd = Vec::new();
        mvhd.extend_from_slice(&[0, 0, 0, 0]); // version/flags
        mvhd.extend_from_slice(&[0, 0, 0, 0]); // creation
        mvhd.extend_from_slice(&[0, 0, 0, 0]); // modification
        mvhd.extend_from_slice(&600u32.to_be_bytes()); // timescale
        mvhd.extend_from_slice(&1200u32.to_be_bytes()); // duration
        let mvhd_box = box_with(b"mvhd", &mvhd);
        let moov_box = box_with(b"moov", &mvhd_box);
        let ftyp_box = box_with(b"ftyp", b"isom\0\0\x02\0isomiso2");
        let mut out = ftyp_box;
        out.extend_from_slice(&moov_box);
        out
    }

    fn box_with(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let size = (body.len() + 8) as u32;
        let mut out = size.to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn reads_mp4_duration() {
        let mp4 = minimal_mp4();
        let info = probe_media(&mp4, "mp4");
        assert!(info.container.contains("MP4"));
        assert!((info.duration_secs - 2.0).abs() < 1e-6, "dur {}", info.duration_secs);
    }

    #[test]
    fn reads_matroska_codec_and_size() {
        // EBML header + Segment > Tracks > TrackEntry(video, V_VP9,
        // 640x480). Sizes are 1-byte vints (values < 127).
        fn elem(id: &[u8], body: &[u8]) -> Vec<u8> {
            let mut out = id.to_vec();
            out.push(0x80 | body.len() as u8); // 1-byte size vint
            out.extend_from_slice(body);
            out
        }
        let width = elem(&[0xB0], &[640u16.to_be_bytes()[0], 640u16.to_be_bytes()[1]]);
        let height = elem(&[0xBA], &[480u16.to_be_bytes()[0], 480u16.to_be_bytes()[1]]);
        let mut video = Vec::new();
        video.extend_from_slice(&width);
        video.extend_from_slice(&height);
        let video_el = elem(&[0xE0], &video);
        let codec = elem(&[0x86], b"V_VP9");
        let mut track = codec;
        track.extend_from_slice(&video_el);
        let track_el = elem(&[0xAE], &track);
        let tracks = elem(&[0x16, 0x54, 0xAE, 0x6B], &track_el);
        let segment = elem(&[0x18, 0x53, 0x80, 0x67], &tracks);
        let mut mkv = vec![0x1A, 0x45, 0xDF, 0xA3, 0x80]; // EBML header, empty
        mkv.extend_from_slice(&segment);

        let info = probe_media(&mkv, "webm");
        assert_eq!(info.video_codec, "VP9");
        assert_eq!((info.width, info.height), (640, 480));
        assert_eq!(info.track_count, 1);
    }
}
