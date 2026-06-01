//! A from-scratch ZIP reader: enough to list a ZIP's entries and extract
//! them (stored or DEFLATE-compressed). Because a `.docx`/`.xlsx`/`.pptx`
//! is just a ZIP of XML, this also powers DrDrOS's Office-document text
//! extraction — pull `word/document.xml` out and strip the tags.

use crate::inflate::inflate;

/// One file inside a ZIP archive.
#[derive(Debug, Clone)]
pub struct ZipEntry {
    pub name: String,
    /// 0 = stored, 8 = DEFLATE.
    pub method: u16,
    pub comp_size: u32,
    pub uncomp_size: u32,
    /// Offset of the local file header within the archive.
    pub local_header: u32,
}

const EOCD_SIG: u32 = 0x0605_4b50;
const CDH_SIG: u32 = 0x0201_4b50;
const LFH_SIG: u32 = 0x0403_4b50;

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// List every entry in a ZIP archive by reading its central directory.
pub fn list_zip(data: &[u8]) -> Result<Vec<ZipEntry>, String> {
    if data.len() < 22 {
        return Err("zip: too small".into());
    }
    // Find the End Of Central Directory record, scanning back from the end
    // (it may be followed by up to 64 KiB of comment).
    let mut eocd = None;
    let start = data.len().saturating_sub(22 + 65_536);
    for i in (start..=data.len() - 22).rev() {
        if u32le(data, i) == EOCD_SIG {
            eocd = Some(i);
            break;
        }
    }
    let eocd = eocd.ok_or("zip: no end-of-central-directory record")?;
    let count = u16le(data, eocd + 10) as usize;
    let cd_off = u32le(data, eocd + 16) as usize;

    let mut entries = Vec::with_capacity(count);
    let mut pos = cd_off;
    for _ in 0..count {
        if pos + 46 > data.len() || u32le(data, pos) != CDH_SIG {
            break;
        }
        let method = u16le(data, pos + 10);
        let comp_size = u32le(data, pos + 20);
        let uncomp_size = u32le(data, pos + 24);
        let name_len = u16le(data, pos + 28) as usize;
        let extra_len = u16le(data, pos + 30) as usize;
        let comment_len = u16le(data, pos + 32) as usize;
        let local_header = u32le(data, pos + 42);
        let name_start = pos + 46;
        if name_start + name_len > data.len() {
            break;
        }
        let name = String::from_utf8_lossy(&data[name_start..name_start + name_len]).into_owned();
        entries.push(ZipEntry { name, method, comp_size, uncomp_size, local_header });
        pos = name_start + name_len + extra_len + comment_len;
    }
    Ok(entries)
}

/// Extract one entry's bytes (stored or inflated).
pub fn read_zip_entry(data: &[u8], entry: &ZipEntry) -> Result<Vec<u8>, String> {
    let lh = entry.local_header as usize;
    if lh + 30 > data.len() || u32le(data, lh) != LFH_SIG {
        return Err("zip: bad local file header".into());
    }
    let name_len = u16le(data, lh + 26) as usize;
    let extra_len = u16le(data, lh + 28) as usize;
    let data_start = lh + 30 + name_len + extra_len;
    let end = data_start + entry.comp_size as usize;
    if end > data.len() {
        return Err("zip: entry data out of range".into());
    }
    let raw = &data[data_start..end];
    match entry.method {
        0 => Ok(raw.to_vec()),
        8 => inflate(raw),
        m => Err(format!("zip: unsupported method {m}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A ZIP built by Python: hello.txt (stored) + big.txt (deflated 'X'*2000).
    const ZIP: &[u8] = &[
        80, 75, 3, 4, 20, 0, 0, 0, 0, 0, 26, 38, 193, 92, 180, 65, 160, 22, 18, 0, 0, 0, 18, 0, 0,
        0, 9, 0, 0, 0, 104, 101, 108, 108, 111, 46, 116, 120, 116, 72, 101, 108, 108, 111, 32, 102,
        114, 111, 109, 32, 97, 32, 90, 73, 80, 33, 10, 80, 75, 3, 4, 20, 0, 0, 0, 8, 0, 0, 0, 33,
        0, 20, 91, 215, 126, 17, 0, 0, 0, 208, 7, 0, 0, 7, 0, 0, 0, 98, 105, 103, 46, 116, 120,
        116, 139, 136, 24, 5, 163, 96, 20, 140, 130, 81, 48, 10, 70, 193, 80, 7, 0, 80, 75, 1, 2,
        20, 3, 20, 0, 0, 0, 0, 0, 26, 38, 193, 92, 180, 65, 160, 22, 18, 0, 0, 0, 18, 0, 0, 0, 9,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 128, 1, 0, 0, 0, 0, 104, 101, 108, 108, 111, 46, 116, 120,
        116, 80, 75, 1, 2, 20, 3, 20, 0, 0, 0, 8, 0, 0, 0, 33, 0, 20, 91, 215, 126, 17, 0, 0, 0,
        208, 7, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 128, 1, 57, 0, 0, 0, 98, 105, 103, 46,
        116, 120, 116, 80, 75, 5, 6, 0, 0, 0, 0, 2, 0, 2, 0, 108, 0, 0, 0, 111, 0, 0, 0, 0, 0,
    ];

    #[test]
    fn lists_and_extracts_stored_and_deflated() {
        let entries = list_zip(ZIP).expect("zip should list");
        assert_eq!(entries.len(), 2);
        let hello = entries.iter().find(|e| e.name == "hello.txt").unwrap();
        assert_eq!(read_zip_entry(ZIP, hello).unwrap(), b"Hello from a ZIP!\n");
        let big = entries.iter().find(|e| e.name == "big.txt").unwrap();
        assert_eq!(big.method, 8); // DEFLATE
        let body = read_zip_entry(ZIP, big).unwrap();
        assert_eq!(body.len(), 2000);
        assert!(body.iter().all(|&b| b == b'X'));
    }
}
