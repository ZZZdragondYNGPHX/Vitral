use super::{corpus_bloom_hashes, pack_word_u128, CORPUS_BLOOM_BITS, CORPUS_BLOOM_WORDS};
use memmap2::Mmap;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

pub(super) type Fingerprint = [u8; 32];

const MAGIC: &[u8; 8] = b"VTRLCORP";
const VERSION: u32 = 1;
const SOURCE_END: usize = 48;
const CHECKSUM_END: usize = SOURCE_END + 32;
const SECTION_COUNT: usize = Section::ContinuationValues as usize + 1;
const HEADER_LEN: usize = CHECKSUM_END + 4 * SECTION_COUNT;
const BUCKET_ROW: usize = 12;
const WORD_ROW: usize = 20;
pub(super) const CHAR_VALUE: usize = 10;
pub(super) const WEIGHTED_VALUE: usize = 12;

#[derive(Clone, Copy)]
pub(super) enum Section {
    Strings,
    StringOffsets,
    Syllables,
    Bloom,
    Chars,
    CharValues,
    Words,
    WordSyllables,
    Readings,
    ReadingValues,
    Initials,
    InitialValues,
    Abbreviations,
    AbbreviationValues,
    Continuations,
    ContinuationValues,
}

use Section::*;

pub(super) enum CorpusBacking {
    Owned(Vec<u8>),
    Mmap(Mmap),
}

impl AsRef<[u8]> for CorpusBacking {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
            Self::Mmap(bytes) => bytes,
        }
    }
}

pub struct RimeDictCorpus {
    backing: CorpusBacking,
    ends: [usize; SECTION_COUNT],
    max_word_chars: usize,
}

impl fmt::Debug for RimeDictCorpus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RimeDictCorpus")
            .field("bytes", &self.bytes().len())
            .field("max_word_chars", &self.max_word_chars)
            .finish()
    }
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn checksum(bytes: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(&bytes[..SOURCE_END]);
    hash.update(&bytes[CHECKSUM_END..]);
    hash.finalize().into()
}

fn matches_source(bytes: &[u8], fingerprint: &Fingerprint) -> bool {
    bytes.len() >= SOURCE_END
        && &bytes[..8] == MAGIC
        && u32_at(bytes, 8) == VERSION
        && &bytes[16..SOURCE_END] == fingerprint
}

fn find_row<const WIDTH: usize>(rows: &[u8], cmp: impl Fn(&[u8]) -> Ordering) -> Option<&[u8]> {
    let (mut low, mut high) = (0, rows.len() / WIDTH);
    while low < high {
        let mid = low + (high - low) / 2;
        let row = &rows[mid * WIDTH..][..WIDTH];
        match cmp(row) {
            Ordering::Less => low = mid + 1,
            Ordering::Greater => high = mid,
            Ordering::Equal => return Some(row),
        }
    }
    None
}

impl RimeDictCorpus {
    pub(super) fn open(path: &Path, fingerprint: &Fingerprint) -> Option<Self> {
        let mut file = File::open(path).ok()?;
        if !Self::file_matches_source(&mut file, fingerprint) {
            return None;
        }
        // Cache writers publish a new inode via rename and never write or truncate a
        // published inode. Existing readers keep their original read-only mapping.
        let mmap = unsafe { Mmap::map(&file) }.ok()?;
        Self::validate(CorpusBacking::Mmap(mmap), fingerprint)
    }

    pub(super) fn file_matches_source(file: &mut File, fingerprint: &Fingerprint) -> bool {
        let mut header = [0; SOURCE_END];
        file.read_exact(&mut header).is_ok() && matches_source(&header, fingerprint)
    }

    fn validate(backing: CorpusBacking, fingerprint: &Fingerprint) -> Option<Self> {
        let bytes = backing.as_ref();
        if bytes.len() < HEADER_LEN || !matches_source(bytes, fingerprint) {
            return None;
        }
        let max_word_chars = u32_at(bytes, 12) as usize;
        if !(1..=12).contains(&max_word_chars) {
            return None;
        }
        let mut ends = [0; SECTION_COUNT];
        let mut start = HEADER_LEN;
        for (idx, end) in ends.iter_mut().enumerate() {
            *end = u32_at(bytes, CHECKSUM_END + idx * 4) as usize;
            if *end < start || *end > bytes.len() {
                return None;
            }
            start = *end;
        }
        if start != bytes.len() || checksum(bytes) != bytes[SOURCE_END..CHECKSUM_END] {
            return None;
        }

        let corpus = Self {
            backing,
            ends,
            max_word_chars,
        };
        corpus.valid_tables().then_some(corpus)
    }

    fn valid_tables(&self) -> bool {
        let Ok(strings) = std::str::from_utf8(self.section(Strings)) else {
            return false;
        };
        let offsets = self.section(StringOffsets);
        let syllables = self.section(Syllables);
        if offsets.len() < 4
            || offsets.len() % 4 != 0
            || u32_at(offsets, 0) != 0
            || syllables.len() % 4 != 0
            || syllables.len() / 4 > usize::from(u16::MAX) + 1
            || self.section(Bloom).len() != CORPUS_BLOOM_WORDS * 8
        {
            return false;
        }
        let mut previous = 0;
        for offset in offsets[4..].chunks_exact(4) {
            let end = u32_at(offset, 0) as usize;
            if end <= previous || !strings.is_char_boundary(end) {
                return false;
            }
            previous = end;
        }
        if previous != strings.len() {
            return false;
        }

        let string_count = offsets.len() / 4 - 1;
        let valid_string = |row: &[u8]| (u32_at(row, 0) as usize) < string_count;
        let valid_syllable = |row: &[u8]| usize::from(u16_at(row, 0)) < syllables.len() / 4;
        if !syllables.chunks_exact(4).all(valid_string) {
            return false;
        }

        for (keys, values, value_width, limit) in [
            (Chars, CharValues, CHAR_VALUE, usize::MAX),
            (Words, WordSyllables, 2, usize::MAX),
            (Readings, ReadingValues, WEIGHTED_VALUE, 120),
            (Initials, InitialValues, 4, 128),
            (Abbreviations, AbbreviationValues, WEIGHTED_VALUE, 16),
            (Continuations, ContinuationValues, WEIGHTED_VALUE, 24),
        ] {
            let rows = self.section(keys);
            let data = self.section(values);
            let key_width = if matches!(keys, Words) {
                WORD_ROW
            } else {
                BUCKET_ROW
            };
            if rows.len() % key_width != 0 || data.len() % value_width != 0 {
                return false;
            }
            let mut previous_char = None;
            let mut previous_key = None;
            let value_count = data.len() / value_width;
            let mut next = 0;
            for row in rows.chunks_exact(key_width) {
                let id = u32_at(row, 0);
                let start = u32_at(row, 4) as usize;
                let len = u32_at(row, 8) as usize;
                if start != next || len == 0 || len > limit || len > value_count - next {
                    return false;
                }
                next += len;
                if matches!(keys, Chars) {
                    if char::from_u32(id).is_none() || previous_char.is_some_and(|prev| prev >= id)
                    {
                        return false;
                    }
                    previous_char = Some(id);
                } else {
                    if !valid_string(row) {
                        return false;
                    }
                    let key = self.string(id);
                    if previous_key.is_some_and(|prev| prev >= key)
                        || (matches!(keys, Words) && key.chars().count() != len)
                    {
                        return false;
                    }
                    previous_key = Some(key);
                }
            }
            if next != value_count {
                return false;
            }
            let valid = data.chunks_exact(value_width).all(|row| match values {
                CharValues | WordSyllables => valid_syllable(row),
                _ => valid_string(row),
            });
            if !valid {
                return false;
            }
        }
        true
    }

    fn bytes(&self) -> &[u8] {
        self.backing.as_ref()
    }

    #[cfg(test)]
    pub(super) fn is_mapped(&self) -> bool {
        matches!(self.backing, CorpusBacking::Mmap(_))
    }

    fn section(&self, section: Section) -> &[u8] {
        let idx = section as usize;
        let start = if idx == 0 {
            HEADER_LEN
        } else {
            self.ends[idx - 1]
        };
        &self.bytes()[start..self.ends[idx]]
    }

    fn string(&self, id: u32) -> &str {
        let offsets = self.section(StringOffsets);
        let index = id as usize * 4;
        let start = u32_at(offsets, index) as usize;
        let end = u32_at(offsets, index + 4) as usize;
        self.section(Strings)
            .get(start..end)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .unwrap_or("")
    }

    fn syllable(&self, id: u16) -> &str {
        self.string(u32_at(self.section(Syllables), usize::from(id) * 4))
    }

    fn bucket(&self, section: Section, row: Option<&[u8]>, width: usize) -> &[u8] {
        let Some(row) = row else { return &[] };
        let start = u32_at(row, 4) as usize * width;
        let len = u32_at(row, 8) as usize * width;
        &self.section(section)[start..start + len]
    }

    fn keyed_row<const WIDTH: usize>(&self, section: Section, key: &str) -> Option<&[u8]> {
        find_row::<WIDTH>(self.section(section), |row| {
            self.string(u32_at(row, 0)).cmp(key)
        })
    }

    fn weighted_bucket(
        &self,
        keys: Section,
        values: Section,
        key: &str,
    ) -> impl ExactSizeIterator<Item = (&str, u64)> + Clone {
        let row = self.keyed_row::<BUCKET_ROW>(keys, key);
        self.bucket(values, row, WEIGHTED_VALUE)
            .chunks_exact(WEIGHTED_VALUE)
            .map(|entry| (self.string(u32_at(entry, 0)), u64_at(entry, 4)))
    }

    #[cfg(test)]
    pub(crate) fn character_keys(&self) -> impl Iterator<Item = char> + '_ {
        self.section(Chars)
            .chunks_exact(BUCKET_ROW)
            .map(|row| char::from_u32(u32_at(row, 0)).unwrap())
    }

    #[cfg(test)]
    pub(crate) fn word_weights(&self) -> impl Iterator<Item = (&str, u64)> {
        self.section(Words)
            .chunks_exact(WORD_ROW)
            .map(|row| (self.string(u32_at(row, 0)), u64_at(row, 12)))
    }

    #[cfg(test)]
    pub(crate) fn continuation_bucket_lengths(&self) -> impl Iterator<Item = usize> + '_ {
        self.section(Continuations)
            .chunks_exact(BUCKET_ROW)
            .map(|row| u32_at(row, 8) as usize)
    }

    pub fn max_word_chars(&self) -> usize {
        self.max_word_chars
    }

    pub fn char_readings(&self, ch: char) -> impl ExactSizeIterator<Item = (&str, u64)> + Clone {
        let row =
            find_row::<BUCKET_ROW>(self.section(Chars), |row| u32_at(row, 0).cmp(&(ch as u32)));
        self.bucket(CharValues, row, CHAR_VALUE)
            .chunks_exact(CHAR_VALUE)
            .map(|entry| (self.syllable(u16_at(entry, 0)), u64_at(entry, 2)))
    }

    pub fn word_reading(
        &self,
        word: &str,
    ) -> Option<(impl ExactSizeIterator<Item = &str> + Clone, u64)> {
        let row = self.keyed_row::<WORD_ROW>(Words, word)?;
        let syllables = self
            .bucket(WordSyllables, Some(row), 2)
            .chunks_exact(2)
            .map(|entry| self.syllable(u16_at(entry, 0)));
        Some((syllables, u64_at(row, 12)))
    }

    pub fn word_weight(&self, word: &str) -> Option<u64> {
        self.keyed_row::<WORD_ROW>(Words, word)
            .map(|row| u64_at(row, 12))
    }

    pub fn spaced_pinyin_words(
        &self,
        reading: &str,
    ) -> impl ExactSizeIterator<Item = (&str, u64)> + Clone {
        self.weighted_bucket(Readings, ReadingValues, reading)
    }

    pub fn initial_readings(&self, initials: &str) -> impl ExactSizeIterator<Item = &str> + Clone {
        let row = self.keyed_row::<BUCKET_ROW>(Initials, initials);
        self.bucket(InitialValues, row, 4)
            .chunks_exact(4)
            .map(|entry| self.string(u32_at(entry, 0)))
    }

    pub fn abbreviations(
        &self,
        initials: &str,
    ) -> impl ExactSizeIterator<Item = (&str, u64)> + Clone {
        self.weighted_bucket(Abbreviations, AbbreviationValues, initials)
    }

    pub fn continuations(
        &self,
        prefix: &str,
    ) -> impl ExactSizeIterator<Item = (&str, u64)> + Clone {
        self.weighted_bucket(Continuations, ContinuationValues, prefix)
    }

    pub fn may_contain_chars(&self, chars: &[char]) -> bool {
        if chars.is_empty() {
            return false;
        }
        let Some(key) = pack_word_u128(chars) else {
            return true;
        };
        let (h1, h2) = corpus_bloom_hashes(key);
        let bloom = self.section(Bloom);
        (0..4u64).all(|probe| {
            let bit = h1.wrapping_add(probe.wrapping_mul(h2)) & (CORPUS_BLOOM_BITS - 1);
            u64_at(bloom, (bit >> 6) as usize * 8) & (1u64 << (bit & 63)) != 0
        })
    }

    pub fn lookup_word_chars(&self, chars: &[char]) -> Option<u64> {
        if !self.may_contain_chars(chars) {
            return None;
        }
        if chars.len() <= 12 {
            let mut buffer = [0; 48];
            let mut end = 0;
            for &ch in chars {
                end += ch.encode_utf8(&mut buffer[end..]).len();
            }
            // encode_utf8 writes whole UTF-8 scalars.
            return self.word_weight(unsafe { std::str::from_utf8_unchecked(&buffer[..end]) });
        }
        find_row::<WORD_ROW>(self.section(Words), |row| {
            self.string(u32_at(row, 0))
                .chars()
                .cmp(chars.iter().copied())
        })
        .map(|row| u64_at(row, 12))
    }

    pub fn lookup_joined_str(&self, first: &str, second: &str) -> Option<u64> {
        let total = first.len() + second.len();
        if total == 0 {
            return None;
        }
        if total <= 64 {
            let mut buffer = [0; 64];
            buffer[..first.len()].copy_from_slice(first.as_bytes());
            buffer[first.len()..total].copy_from_slice(second.as_bytes());
            // Both inputs end on UTF-8 scalar boundaries.
            let word = unsafe { std::str::from_utf8_unchecked(&buffer[..total]) };
            let mut chars = ['\0'; 6];
            let mut count = 0;
            for ch in word.chars().take(chars.len()) {
                chars[count] = ch;
                count += 1;
            }
            return (count > 5 || self.may_contain_chars(&chars[..count]))
                .then(|| self.word_weight(word))
                .flatten();
        }
        find_row::<WORD_ROW>(self.section(Words), |row| {
            self.string(u32_at(row, 0))
                .bytes()
                .cmp(first.bytes().chain(second.bytes()))
        })
        .map(|row| u64_at(row, 12))
    }
}

// The header stores the source fingerprint, checksum, and each section's end
// offset. Tables follow in Section order. String IDs index adjacent offsets in
// the UTF-8 pool. Bucket rows hold a key ID, first value index, and value count.
pub(super) struct CorpusWriter {
    sections: [Vec<u8>; SECTION_COUNT],
}

impl CorpusWriter {
    pub(super) fn new() -> Self {
        let mut sections = std::array::from_fn(|_| Vec::new());
        sections[StringOffsets as usize] = 0u32.to_le_bytes().to_vec();
        Self { sections }
    }

    pub(super) fn section(&mut self, section: Section) -> &mut Vec<u8> {
        &mut self.sections[section as usize]
    }

    pub(super) fn string(&mut self, text: &str) -> u32 {
        debug_assert!(!text.is_empty());
        let pool = self.section(Strings);
        pool.extend_from_slice(text.as_bytes());
        let end = as_u32(pool.len());
        let offsets = self.section(StringOffsets);
        let id = as_u32(offsets.len() / 4 - 1);
        offsets.extend_from_slice(&end.to_le_bytes());
        id
    }

    pub(super) fn row(&mut self, section: Section, key: u32, start: usize, len: usize) {
        let rows = self.section(section);
        for value in [key, as_u32(start), as_u32(len)] {
            rows.extend_from_slice(&value.to_le_bytes());
        }
    }

    pub(super) fn weighted(&mut self, section: Section, value: u32, weight: u64) {
        let entries = self.section(section);
        entries.extend_from_slice(&value.to_le_bytes());
        entries.extend_from_slice(&weight.to_le_bytes());
    }

    pub(super) fn finish(self, fingerprint: &Fingerprint, max_word_chars: usize) -> RimeDictCorpus {
        let total = HEADER_LEN + self.sections.iter().map(Vec::len).sum::<usize>();
        let mut bytes = Vec::with_capacity(total);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&as_u32(max_word_chars).to_le_bytes());
        bytes.extend_from_slice(fingerprint);
        bytes.resize(HEADER_LEN, 0);
        let mut ends = [0; SECTION_COUNT];
        for (idx, section) in self.sections.into_iter().enumerate() {
            bytes.extend_from_slice(&section);
            ends[idx] = bytes.len();
            let field = CHECKSUM_END + idx * 4;
            bytes[field..field + 4].copy_from_slice(&as_u32(ends[idx]).to_le_bytes());
        }
        let digest = checksum(&bytes);
        bytes[SOURCE_END..CHECKSUM_END].copy_from_slice(&digest);
        RimeDictCorpus {
            backing: CorpusBacking::Owned(bytes),
            ends,
            max_word_chars,
        }
    }
}

impl RimeDictCorpus {
    pub(super) fn write_atomic(&self, path: &Path) -> io::Result<()> {
        use std::io::Write;
        let dir = path.parent().unwrap();
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
        let mut nonce = [0u8; 8];
        getrandom::getrandom(&mut nonce).map_err(|error| io::Error::other(error.to_string()))?;
        let temporary = path.with_extension(format!("{:016x}.tmp", u64::from_le_bytes(nonce)));
        let mut file = File::options()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let result = (|| {
            file.write_all(self.bytes())?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temporary, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temporary);
        }
        result
    }
}

pub(super) fn as_u32(value: usize) -> u32 {
    u32::try_from(value).expect("Rime corpus offset exceeds 32 bits")
}

#[cfg(test)]
pub(super) fn invalid_layouts(valid: &[u8]) -> [Vec<u8>; 3] {
    let mut version = valid.to_vec();
    version[8..12].copy_from_slice(&(VERSION + 1).to_le_bytes());
    let mut offset = valid.to_vec();
    offset[CHECKSUM_END..CHECKSUM_END + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    let mut utf8 = valid.to_vec();
    utf8[HEADER_LEN] = 0xff;
    for bytes in [&mut offset, &mut utf8] {
        let digest = checksum(bytes);
        bytes[SOURCE_END..CHECKSUM_END].copy_from_slice(&digest);
    }
    [version, offset, utf8]
}
