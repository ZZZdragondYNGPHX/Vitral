use crate::error::VitralError;
use memmap2::Mmap;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

const GRAM_HEADER_LEN: usize = 44;
const GRAM_MAGIC: &[u8] = b"Rime::Grammar/1.0\0";

#[derive(Clone, Copy, Debug)]
pub struct EncodedChar {
    pub bytes: [u8; 5],
    pub len: u8,
}

#[inline]
pub fn encode_char(ch: char) -> EncodedChar {
    let cp = ch as u32;
    let mut bytes = [0u8; 5];
    let len = if cp == 0 {
        bytes[0] = 0xe0;
        1
    } else if cp <= 0x7f {
        bytes[0] = cp as u8;
        1
    } else if (0x4000..=0x9fff).contains(&cp) {
        let hi = ((cp >> 8) + 0x40) as u8;
        let lo = (cp & 0xff) as u8;
        if lo == 0 {
            bytes[0] = 0xe1;
            bytes[1] = hi;
        } else {
            bytes[0] = hi;
            bytes[1] = lo;
        }
        2
    } else if cp < 0x800 {
        bytes[0] = 0xe2;
        bytes[1] = (0x80 | ((cp >> 6) & 0x3f)) as u8;
        bytes[2] = (0x80 | (cp & 0x3f)) as u8;
        3
    } else if cp < 0x1_0000 {
        bytes[0] = 0xe3;
        bytes[1] = (0x80 | ((cp >> 12) & 0x0f)) as u8;
        bytes[2] = (0x80 | ((cp >> 6) & 0x3f)) as u8;
        bytes[3] = (0x80 | (cp & 0x3f)) as u8;
        4
    } else {
        bytes[0] = 0xe4;
        bytes[1] = (0x80 | ((cp >> 18) & 0x07)) as u8;
        bytes[2] = (0x80 | ((cp >> 12) & 0x3f)) as u8;
        bytes[3] = (0x80 | ((cp >> 6) & 0x3f)) as u8;
        bytes[4] = (0x80 | (cp & 0x3f)) as u8;
        5
    };
    EncodedChar { bytes, len }
}

/// Pure-Rust memory-mapped reader for `wanxiang-lts-zh-hans.gram` (`Rime::Grammar/1.0` + `Darts::DoubleArray`).
#[derive(Debug)]
pub struct RimeGramDb {
    mmap: Mmap,
    units_len: usize,
    body_offset: usize,
}

static GRAM_CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<RimeGramDb>>>> = OnceLock::new();

impl RimeGramDb {
    pub fn clear_cache() { if let Some(c) = GRAM_CACHE.get() { c.lock().clear(); } }
    pub fn open_cached(path: &Path) -> Result<Arc<Self>, VitralError> {
        let canon = path.to_path_buf();
        let cache = GRAM_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        let mut guard = cache.lock();
        if let Some(db) = guard.get(&canon) {
            return Ok(Arc::clone(db));
        }
        let opened = Arc::new(Self::open(path)?);
        guard.insert(canon, Arc::clone(&opened));
        Ok(opened)
    }

    pub fn open(path: &Path) -> Result<Self, VitralError> {
        let file = File::open(path).map_err(|e| VitralError::Io(e.to_string()))?;
        let mmap = unsafe { Mmap::map(&file) }.map_err(|e| VitralError::Io(e.to_string()))?;
        if mmap.len() < GRAM_HEADER_LEN {
            return Err(VitralError::Parse("gram file shorter than 44-byte header".into()));
        }
        if !mmap[0..GRAM_MAGIC.len()].starts_with(GRAM_MAGIC) {
            return Err(VitralError::Parse("invalid Rime::Grammar/1.0 magic".into()));
        }
        let units_len = u32::from_le_bytes([mmap[0x24], mmap[0x25], mmap[0x26], mmap[0x27]]) as usize;
        let rel_off = i32::from_le_bytes([mmap[0x28], mmap[0x29], mmap[0x2a], mmap[0x2b]]);
        let body_offset_isize = 0x28isize + (rel_off as isize);
        if body_offset_isize < GRAM_HEADER_LEN as isize {
            return Err(VitralError::Parse(format!(
                "invalid gram body_offset {body_offset_isize} (< {GRAM_HEADER_LEN})"
            )));
        }
        let body_offset = body_offset_isize as usize;
        let body_bytes = units_len
            .checked_mul(4)
            .ok_or_else(|| VitralError::Parse("gram units_len overflow".into()))?;
        let needed = body_offset
            .checked_add(body_bytes)
            .ok_or_else(|| VitralError::Parse("gram size overflow".into()))?;
        if needed > mmap.len() {
            return Err(VitralError::Parse(format!(
                "gram file truncated: expected {needed} bytes, found {}",
                mmap.len()
            )));
        }
        Ok(Self {
            mmap,
            units_len,
            body_offset,
        })
    }

    #[inline]
    fn unit_at(&self, idx: usize) -> Option<u32> {
        if idx >= self.units_len {
            return None;
        }
        let off = self.body_offset + idx * 4;
        let b = &self.mmap[off..off + 4];
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    #[cfg(target_arch = "x86_64")]
    #[inline(always)]
    fn prefetch_unit(&self, idx: usize) {
        if idx < self.units_len {
            unsafe {
                let ptr = self.mmap.as_ptr().add(self.body_offset + idx * 4) as *const i8;
                std::arch::x86_64::_mm_prefetch(ptr, std::arch::x86_64::_MM_HINT_T0);
            }
        }
    }

    #[inline]
    pub fn root_state(&self) -> Option<(usize, u32)> {
        if self.units_len == 0 {
            return None;
        }
        Some((0, self.unit_at(0)?))
    }

    #[inline]
    pub fn advance_state_chars(
        &self,
        mut node_pos: usize,
        mut unit: u32,
        chars: &[char],
    ) -> Option<(usize, u32)> {
        if self.units_len == 0 {
            return None;
        }
        for &ch in chars {
            let ec = encode_char(ch);
            for &b in &ec.bytes[..ec.len as usize] {
                let shift = ((unit >> 6) & 8) as usize;
                let offset = ((unit >> 10) as usize) << shift;
                node_pos = node_pos ^ offset ^ (b as usize);
                unit = self.unit_at(node_pos)?;
                if (unit & 0x8000_00ff) != (b as u32) {
                    return None;
                }
            }
        }
        Some((node_pos, unit))
    }

    #[inline]
    pub fn prefix_state_chars(&self, chars: &[char]) -> Option<(usize, u32)> {
        if chars.is_empty() || self.units_len == 0 {
            return None;
        }
        let root_unit = self.unit_at(0)?;
        self.advance_state_chars(0, root_unit, chars)
    }

    #[inline]
    pub fn lookup_from_state_1char(&self, mut node_pos: usize, mut unit: u32, ch: char) -> Option<f32> {
        if self.units_len == 0 {
            return None;
        }
        let ec = encode_char(ch);
        for &b in &ec.bytes[..ec.len as usize] {
            let shift = ((unit >> 6) & 8) as usize;
            let offset = ((unit >> 10) as usize) << shift;
            node_pos = node_pos ^ offset ^ (b as usize);
            unit = self.unit_at(node_pos)?;
            if (unit & 0x8000_00ff) != (b as u32) {
                return None;
            }
        }
        if ((unit >> 8) & 1) == 0 {
            return None;
        }
        let shift = ((unit >> 6) & 8) as usize;
        let offset = ((unit >> 10) as usize) << shift;
        let leaf_pos = node_pos ^ offset;
        let leaf_unit = self.unit_at(leaf_pos)?;
        let val = (leaf_unit & 0x7fff_ffff) as i32;
        Some(((val as f64) / 10000.0) as f32)
    }

    #[inline]
    pub fn lookup_from_state_chars(
        &self,
        node_pos: usize,
        unit: u32,
        chars: &[char],
    ) -> Option<f32> {
        if chars.is_empty() || self.units_len == 0 {
            return None;
        }
        let (np, u) = self.advance_state_chars(node_pos, unit, chars)?;
        if ((u >> 8) & 1) == 0 {
            return None;
        }
        let shift = ((u >> 6) & 8) as usize;
        let offset = ((u >> 10) as usize) << shift;
        let leaf_pos = np ^ offset;
        let leaf_unit = self.unit_at(leaf_pos)?;
        let val = (leaf_unit & 0x7fff_ffff) as i32;
        Some(((val as f64) / 10000.0) as f32)
    }

    #[inline]
    pub fn lookup_from_state_batch8(
        &self,
        node_pos: usize,
        unit: u32,
        enc: &[EncodedChar; 8],
        cnt: usize,
        active_mask: u8,
    ) -> [f32; 8] {
        let mut out = [0.0f32; 8];
        if self.units_len == 0 || active_mask == 0 || cnt == 0 {
            return out;
        }
        let n = cnt.min(8);
        let shift0 = ((unit >> 6) & 8) as usize;
        let offset0 = ((unit >> 10) as usize) << shift0;
        let base0 = node_pos ^ offset0;

        let mut pos_arr = [0usize; 8];
        let mut alive = 0u8;
        let mut leaf_mask = 0u8;
        let mut leaf_pos = [0usize; 8];

        // Stage 1: byte-0 address calculation + parallel prefetch
        for k in 0..n {
            if (active_mask & (1 << k)) != 0 && enc[k].len > 0 {
                let p = base0 ^ (enc[k].bytes[0] as usize);
                pos_arr[k] = p;
                alive |= 1 << k;
                #[cfg(target_arch = "x86_64")]
                self.prefetch_unit(p);
            }
        }
        if alive == 0 {
            return out;
        }

        // Stage 2: verify byte-0 + prefetch byte-1 (or 1-byte leaf)
        let mut alive2 = 0u8;
        for k in 0..n {
            if (alive & (1 << k)) == 0 {
                continue;
            }
            let Some(u) = self.unit_at(pos_arr[k]) else {
                continue;
            };
            if (u & 0x8000_00ff) != (enc[k].bytes[0] as u32) {
                continue;
            }
            let shift = ((u >> 6) & 8) as usize;
            let offset = ((u >> 10) as usize) << shift;
            if enc[k].len == 1 {
                if ((u >> 8) & 1) != 0 {
                    let lp = pos_arr[k] ^ offset;
                    leaf_pos[k] = lp;
                    leaf_mask |= 1 << k;
                    #[cfg(target_arch = "x86_64")]
                    self.prefetch_unit(lp);
                }
            } else {
                let p1 = pos_arr[k] ^ offset ^ (enc[k].bytes[1] as usize);
                pos_arr[k] = p1;
                alive2 |= 1 << k;
                #[cfg(target_arch = "x86_64")]
                self.prefetch_unit(p1);
            }
        }

        // Stage 3: verify byte-1 (2-byte CJK BMP fast path) + tail bytes + prefetch leaf
        for k in 0..n {
            if (alive2 & (1 << k)) == 0 {
                continue;
            }
            let Some(mut u) = self.unit_at(pos_arr[k]) else {
                continue;
            };
            if (u & 0x8000_00ff) != (enc[k].bytes[1] as u32) {
                continue;
            }
            let mut np = pos_arr[k];
            let elen = enc[k].len as usize;
            let mut ok = true;
            for &b in enc[k].bytes.iter().take(elen).skip(2) {
                let shift = ((u >> 6) & 8) as usize;
                let offset = ((u >> 10) as usize) << shift;
                np = np ^ offset ^ (b as usize);
                if let Some(nu) = self.unit_at(np) {
                    if (nu & 0x8000_00ff) != (b as u32) {
                        ok = false;
                        break;
                    }
                    u = nu;
                } else {
                    ok = false;
                    break;
                }
            }
            if ok && ((u >> 8) & 1) != 0 {
                let shift = ((u >> 6) & 8) as usize;
                let offset = ((u >> 10) as usize) << shift;
                let lp = np ^ offset;
                leaf_pos[k] = lp;
                leaf_mask |= 1 << k;
                #[cfg(target_arch = "x86_64")]
                self.prefetch_unit(lp);
            }
        }

        // Stage 4: extract leaf scores
        for k in 0..n {
            if (leaf_mask & (1 << k)) != 0 {
                if let Some(lu) = self.unit_at(leaf_pos[k]) {
                    let val = (lu & 0x7fff_ffff) as i32;
                    out[k] = ((val as f64) / 10000.0) as f32;
                }
            }
        }
        out
    }

    /// Exact `rime::grammar::encode` with `0xe1` escape for `(cp & 0xff) == 0` in `0x4000..=0x9FFF`
    /// (e.g. `U+4E00 一` -> `[0xe1, 0x8e]`), avoiding `0x00` Darts terminators.
    pub fn encode_into(text: &str, out: &mut Vec<u8>) {
        out.reserve(text.len());
        for ch in text.chars() {
            let cp = ch as u32;
            if cp == 0 {
                out.push(0xe0);
            } else if cp <= 0x7f {
                out.push(cp as u8);
            } else if (0x4000..=0x9fff).contains(&cp) {
                let hi = ((cp >> 8) + 0x40) as u8;
                let lo = (cp & 0xff) as u8;
                if lo == 0 {
                    out.push(0xe1);
                    out.push(hi);
                } else {
                    out.push(hi);
                    out.push(lo);
                }
            } else if cp < 0x800 {
                out.push(0xe2);
                out.push((0x80 | ((cp >> 6) & 0x3f)) as u8);
                out.push((0x80 | (cp & 0x3f)) as u8);
            } else if cp < 0x1_0000 {
                out.push(0xe3);
                out.push((0x80 | ((cp >> 12) & 0x0f)) as u8);
                out.push((0x80 | ((cp >> 6) & 0x3f)) as u8);
                out.push((0x80 | (cp & 0x3f)) as u8);
            } else {
                out.push(0xe4);
                out.push((0x80 | ((cp >> 18) & 0x07)) as u8);
                out.push((0x80 | ((cp >> 12) & 0x3f)) as u8);
                out.push((0x80 | ((cp >> 6) & 0x3f)) as u8);
                out.push((0x80 | (cp & 0x3f)) as u8);
            }
        }
    }

    pub fn encode(text: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity(text.len());
        Self::encode_into(text, &mut out);
        out
    }

    /// Advance the Darts double-array state across `prefix`, returning `None` immediately
    /// if no key in the `.gram` trie begins with `prefix`.
    pub fn prefix_state(&self, prefix: &str, buf: &mut Vec<u8>) -> Option<(usize, u32)> {
        if prefix.is_empty() || self.units_len == 0 {
            return None;
        }
        buf.clear();
        Self::encode_into(prefix, buf);
        let mut node_pos: usize = 0;
        let mut unit = self.unit_at(0)?;
        for &b in buf.iter() {
            let shift = ((unit >> 6) & 8) as usize;
            let offset = ((unit >> 10) as usize) << shift;
            node_pos = node_pos ^ offset ^ (b as usize);
            unit = self.unit_at(node_pos)?;
            if (unit & 0x8000_00ff) != (b as u32) {
                return None;
            }
        }
        Some((node_pos, unit))
    }

    /// Resume a Darts lookup from a pre-advanced `prefix_state` using `suffix`.
    pub fn lookup_from_state(
        &self,
        mut node_pos: usize,
        mut unit: u32,
        suffix: &str,
        buf: &mut Vec<u8>,
    ) -> Option<f64> {
        if suffix.is_empty() || self.units_len == 0 {
            return None;
        }
        buf.clear();
        Self::encode_into(suffix, buf);
        for &b in buf.iter() {
            let shift = ((unit >> 6) & 8) as usize;
            let offset = ((unit >> 10) as usize) << shift;
            node_pos = node_pos ^ offset ^ (b as usize);
            unit = self.unit_at(node_pos)?;
            if (unit & 0x8000_00ff) != (b as u32) {
                return None;
            }
        }
        if ((unit >> 8) & 1) == 0 {
            return None;
        }
        let shift = ((unit >> 6) & 8) as usize;
        let offset = ((unit >> 10) as usize) << shift;
        let leaf_pos = node_pos ^ offset;
        let leaf_unit = self.unit_at(leaf_pos)?;
        let val = (leaf_unit & 0x7fff_ffff) as i32;
        Some((val as f64) / 10000.0)
    }

    /// Look up `context + word` collocation log-score (`val / 10000.0`) reusing caller `buf`.
    pub fn lookup_with_buf(&self, context: &str, word: &str, buf: &mut Vec<u8>) -> Option<f64> {
        if word.is_empty() || self.units_len == 0 {
            return None;
        }
        buf.clear();
        Self::encode_into(context, buf);
        Self::encode_into(word, buf);
        self.lookup_encoded(buf)
    }

    /// Look up `context + word` collocation log-score (`val / 10000.0`).
    pub fn lookup(&self, context: &str, word: &str) -> Option<f64> {
        if word.is_empty() || self.units_len == 0 {
            return None;
        }
        let mut key = Vec::with_capacity(context.len() + word.len());
        Self::encode_into(context, &mut key);
        Self::encode_into(word, &mut key);
        self.lookup_encoded(&key)
    }

    pub fn lookup_encoded(&self, key: &[u8]) -> Option<f64> {
        if key.is_empty() || self.units_len == 0 {
            return None;
        }
        let mut node_pos: usize = 0;
        let mut unit = self.unit_at(0)?;
        for &b in key {
            let shift = ((unit >> 6) & 8) as usize;
            let offset = ((unit >> 10) as usize) << shift;
            node_pos = node_pos ^ offset ^ (b as usize);
            unit = self.unit_at(node_pos)?;
            if (unit & 0x8000_00ff) != (b as u32) {
                return None;
            }
        }
        if ((unit >> 8) & 1) == 0 {
            return None;
        }
        let shift = ((unit >> 6) & 8) as usize;
        let offset = ((unit >> 10) as usize) << shift;
        let leaf_pos = node_pos ^ offset;
        let leaf_unit = self.unit_at(leaf_pos)?;
        let val = (leaf_unit & 0x7fff_ffff) as i32;
        Some((val as f64) / 10000.0)
    }
}
