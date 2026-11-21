use crate::error::VitralError;
use crate::imp::xiaohe::normalize_syllable;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct RawScelEntry {
    pub text: String,
    pub syllables: Vec<String>,
    pub weight: u32,
}

fn read_u16_le(data: &[u8], offset: usize) -> Result<u16, VitralError> {
    let slice = data
        .get(offset..offset + 2)
        .ok_or_else(|| VitralError::Parse("unexpected EOF in .scel u16".into()))?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32_le(data: &[u8], offset: usize) -> Result<u32, VitralError> {
    let slice = data
        .get(offset..offset + 4)
        .ok_or_else(|| VitralError::Parse("unexpected EOF in .scel u32".into()))?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn decode_utf16_le(bytes: &[u8]) -> Result<String, VitralError> {
    if bytes.len() % 2 != 0 {
        return Err(VitralError::Parse("odd byte length for UTF-16LE string in .scel".into()));
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16(&units)
        .map_err(|e| VitralError::Parse(format!("invalid UTF-16LE in .scel: {e}")))
}

/// Parse a binary Sogou `.scel` file (`0x44 -> 0x2628` and `0x45 -> 0x26C4` variants).
pub fn parse_scel(data: &[u8]) -> Result<Vec<RawScelEntry>, VitralError> {
    if data.len() < 0x2628 {
        return Err(VitralError::Parse(format!(
            ".scel file too short ({} bytes, minimum 9768)",
            data.len()
        )));
    }
    if data[0..4] != [0x40, 0x15, 0x00, 0x00] {
        return Err(VitralError::Parse("invalid .scel magic bytes at 0x00..0x04".into()));
    }
    let word_start: usize = match data[0x04] {
        0x44 => 0x2628,
        0x45 => 0x26C4,
        other => {
            return Err(VitralError::Parse(format!(
                "unsupported .scel version byte 0x{other:02x}"
            )));
        }
    };
    if data[0x05..0x08] != [0x43, 0x53, 0x01] && data[0x04..0x08] != *b"DLES" {
        return Err(VitralError::Parse("invalid .scel signature at 0x05..0x08".into()));
    }
    if data.len() < word_start {
        return Err(VitralError::Parse(format!(
            ".scel truncated before word table offset 0x{word_start:x}"
        )));
    }

    // Parse global pinyin table at 0x1540..word_start
    let mut pos = 0x1540usize;
    let py_header = read_u32_le(data, pos)?;
    let mut py_table: HashMap<u16, String> = HashMap::new();

    // Standard header at 0x1540 is `9d 01 00 00` (413 entries).
    // If the upper 16 bits are 0 and count is reasonable (< 0x1000), skip the 4-byte count header.
    let expected_count = if py_header > 0 && py_header < 0x1000 {
        pos += 4;
        Some(py_header as usize)
    } else {
        None
    };

    let mut parsed_py = 0usize;
    while pos + 4 <= word_start {
        if let Some(limit) = expected_count {
            if parsed_py >= limit {
                break;
            }
        }
        let idx = read_u16_le(data, pos)?;
        let len_bytes = read_u16_le(data, pos + 2)? as usize;
        pos += 4;
        if len_bytes == 0 || len_bytes % 2 != 0 || pos + len_bytes > word_start {
            return Err(VitralError::Parse(format!(
                "corrupt .scel pinyin table entry at 0x{:x}",
                pos - 4
            )));
        }
        let syl_raw = decode_utf16_le(&data[pos..pos + len_bytes])?;
        let syl = normalize_syllable(syl_raw.trim_end_matches('\0'));
        py_table.insert(idx, syl);
        pos += len_bytes;
        parsed_py += 1;
    }

    if py_table.is_empty() {
        return Err(VitralError::Parse("empty .scel pinyin table".into()));
    }

    // Parse homophone word groups from word_start..EOF
    let mut entries = Vec::new();
    let mut wpos = word_start;
    while wpos < data.len() {
        if wpos + 4 > data.len() {
            return Err(VitralError::Parse(format!(
                "truncated .scel word group header at 0x{wpos:x}"
            )));
        }
        let same_py_count = read_u16_le(data, wpos)? as usize;
        let py_idx_bytes_len = read_u16_le(data, wpos + 2)? as usize;
        wpos += 4;

        if py_idx_bytes_len == 0
            || py_idx_bytes_len % 2 != 0
            || wpos + py_idx_bytes_len > data.len()
        {
            return Err(VitralError::Parse(format!(
                "invalid .scel py_idx_bytes_len ({py_idx_bytes_len}) at 0x{:x}",
                wpos - 4
            )));
        }
        let mut syllables = Vec::with_capacity(py_idx_bytes_len / 2);
        for chunk in data[wpos..wpos + py_idx_bytes_len].chunks_exact(2) {
            let py_idx = u16::from_le_bytes([chunk[0], chunk[1]]);
            let syl = py_table.get(&py_idx).ok_or_else(|| {
                VitralError::Parse(format!("unknown .scel pinyin index {py_idx} at 0x{wpos:x}"))
            })?;
            syllables.push(syl.clone());
        }
        wpos += py_idx_bytes_len;

        for _ in 0..same_py_count {
            if wpos + 2 > data.len() {
                return Err(VitralError::Parse(format!(
                    "truncated .scel word length at 0x{wpos:x}"
                )));
            }
            let word_len_bytes = read_u16_le(data, wpos)? as usize;
            wpos += 2;
            if word_len_bytes == 0
                || word_len_bytes % 2 != 0
                || wpos + word_len_bytes > data.len()
            {
                return Err(VitralError::Parse(format!(
                    "invalid .scel word_len_bytes ({word_len_bytes}) at 0x{:x}",
                    wpos - 2
                )));
            }
            let text = decode_utf16_le(&data[wpos..wpos + word_len_bytes])?
                .trim_matches('\0')
                .trim()
                .to_string();
            wpos += word_len_bytes;

            if wpos + 2 > data.len() {
                return Err(VitralError::Parse(format!(
                    "truncated .scel ext_len at 0x{wpos:x}"
                )));
            }
            let ext_len = read_u16_le(data, wpos)? as usize;
            wpos += 2;
            if wpos + ext_len > data.len() {
                return Err(VitralError::Parse(format!(
                    "truncated .scel ext_data ({ext_len} bytes) at 0x{wpos:x}"
                )));
            }
            let weight = if ext_len >= 2 {
                let f = u16::from_le_bytes([data[wpos], data[wpos + 1]]) as u32;
                f.max(1)
            } else {
                100
            };
            wpos += ext_len;

            if !text.is_empty() {
                entries.push(RawScelEntry {
                    text,
                    syllables: syllables.clone(),
                    weight,
                });
            }
        }
    }

    Ok(entries)
}
