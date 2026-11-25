mod compact;

use crate::imp::xiaohe::{normalize_syllable, syllable_to_xiaohe};
use compact::{as_u32, CorpusWriter, Fingerprint, Section::*, CHAR_VALUE, WEIGHTED_VALUE};
use parking_lot::Mutex;
use pinyin::{ToPinyin, ToPinyinMulti};
use sha2::{Digest, Sha256};
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::fs;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

pub use compact::RimeDictCorpus;

const CORPUS_BLOOM_WORDS: usize = 131_072; // 131,072 * 8B = 1 MiB
const CORPUS_BLOOM_BITS: u64 = (CORPUS_BLOOM_WORDS as u64) * 64;

#[inline]
fn pack_word_u128(chars: &[char]) -> Option<u128> {
    if chars.is_empty() || chars.len() > 5 {
        return None;
    }
    let mut k = (chars.len() as u128) << 120;
    for (i, &ch) in chars.iter().enumerate() {
        k |= ((ch as u128) + 1) << (i * 21);
    }
    Some(k)
}

#[inline]
fn mix64(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

#[inline]
fn corpus_bloom_hashes(k: u128) -> (u64, u64) {
    let lo = mix64(k as u64);
    let hi = mix64(((k >> 64) as u64) ^ 0x9e37_79b9_7f4a_7c15);
    let h1 = mix64(lo ^ hi);
    let h2 = mix64(lo.wrapping_add(hi.rotate_left(17)));
    (h1, h2)
}

const DICTIONARIES: [(&str, u64, bool); 5] = [
    ("8105.dict.yaml", 50, true),
    ("41448.dict.yaml", 10, false),
    ("base.dict.yaml", 100, true),
    ("ext.dict.yaml", 80, true),
    ("others.dict.yaml", 25, false),
];

type CorpusKey = (Option<PathBuf>, Fingerprint);
static CORPUS_CACHE: OnceLock<Mutex<HashMap<CorpusKey, Arc<RimeDictCorpus>>>> = OnceLock::new();

fn source_fingerprint(dicts: Option<&Path>) -> Fingerprint {
    let mut hash = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    if let Some(dir) = dicts {
        for (name, _, _) in DICTIONARIES {
            let path = dir.join(name);
            let encoded = path.as_os_str().as_encoded_bytes();
            hash.update((encoded.len() as u64).to_le_bytes());
            hash.update(encoded);
            match File::open(&path) {
                Ok(mut file) => {
                    let mut len = 0u64;
                    hash.update([1]);
                    loop {
                        match file.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                len += n as u64;
                                hash.update(&buf[..n]);
                            }
                            Err(_) => {
                                hash.update([2]);
                                break;
                            }
                        }
                    }
                    hash.update(len.to_le_bytes());
                }
                Err(_) => hash.update([0]),
            }
        }
    }
    hash.finalize().into()
}

impl RimeDictCorpus {
    pub fn load_or_fallback(assets_dir: Option<&Path>) -> Arc<Self> {
        Self::load_or_fallback_with_cache(assets_dir, None)
    }

    pub fn load_or_fallback_with_cache(
        assets_dir: Option<&Path>,
        cache_dir: Option<&Path>,
    ) -> Arc<Self> {
        let assets = assets_dir.map(|path| path.canonicalize().unwrap_or_else(|_| path.to_owned()));
        let dicts = assets.as_ref().map(|dir| dir.join("rime/cn_dicts"));
        let cache = CORPUS_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        let mut guard = cache.lock();

        loop {
            let fingerprint = source_fingerprint(dicts.as_deref());
            let key = (assets.clone(), fingerprint);
            let cache_path = cache_dir
                .filter(|dir| !dir.as_os_str().is_empty())
                .zip(dicts.as_ref().filter(|dir| dir.is_dir()))
                .map(|(dir, dicts)| {
                    let path_hash = Sha256::digest(dicts.as_os_str().as_encoded_bytes());
                    dir.join(format!("rime-corpus-{path_hash:x}.bin"))
                });
            if let Some(corpus) = guard.get(&key) {
                if let Some(path) = &cache_path {
                    if !File::open(path)
                        .is_ok_and(|mut file| Self::file_matches_source(&mut file, &fingerprint))
                    {
                        let _ = corpus.write_atomic(path);
                    }
                }
                return Arc::clone(corpus);
            }
            // Keep active readers alive through their Arcs while dropping the stale cache entry.
            guard.retain(|(dir, _), _| dir != &assets);
            let mapped = cache_path
                .as_ref()
                .and_then(|path| Self::open(path, &fingerprint));
            let (corpus, complete, needs_write) = match mapped {
                Some(corpus) => (corpus, true, false),
                None => {
                    let (builder, complete) =
                        CorpusBuilder::load(dicts.as_deref().filter(|dir| dir.is_dir()));
                    (builder.finish(&fingerprint), complete, true)
                }
            };
            if source_fingerprint(dicts.as_deref()) != fingerprint {
                continue;
            }
            let corpus = Arc::new(corpus);
            if complete {
                if let Some(path) = cache_path.filter(|_| needs_write) {
                    let _ = corpus.write_atomic(&path);
                }
                guard.insert(key, Arc::clone(&corpus));
            }
            return corpus;
        }
    }
}

type WordId = u32;
type SyllableId = u16;
type Weighted<T> = (T, u64);

struct WordReading {
    start: usize,
    len: usize,
    weight: u64,
}

#[derive(Default)]
struct CorpusBuilder {
    syllable_ids: HashMap<Box<str>, SyllableId>,
    syllables: Vec<Box<str>>,
    char_readings: HashMap<char, Vec<Weighted<SyllableId>>>,
    word_ids: HashMap<Box<str>, WordId>,
    words: Vec<WordReading>,
    word_syllables: Vec<SyllableId>,
    homophones: HashMap<Box<str>, Vec<Weighted<WordId>>>,
}

impl CorpusBuilder {
    fn load(dicts: Option<&Path>) -> (Self, bool) {
        let mut excluded = [false; DICTIONARIES.len()];
        loop {
            let mut builder = Self::default();
            if let Some(dir) = dicts {
                let failure =
                    DICTIONARIES
                        .iter()
                        .enumerate()
                        .find(|&(index, &(name, weight, overrides))| {
                            !excluded[index]
                                && builder
                                    .parse_file(&dir.join(name), weight, overrides)
                                    .is_err()
                        });
                if let Some((index, _)) = failure {
                    // Discard a partially read dictionary just as an invalid UTF-8 file is skipped.
                    excluded[index] = true;
                    continue;
                }
            }
            if builder.words.is_empty() {
                builder.seed_core_fallback();
            }
            return (builder, !excluded.contains(&true));
        }
    }

    fn syllable_id(&mut self, raw: &str) -> io::Result<Option<SyllableId>> {
        if let Some(&id) = self.syllable_ids.get(raw) {
            return Ok(Some(id));
        }
        let normalized = normalize_syllable(raw);
        if normalized.is_empty() {
            return Ok(None);
        }
        if let Some(&id) = self.syllable_ids.get(normalized.as_str()) {
            return Ok(Some(id));
        }
        let id = SyllableId::try_from(self.syllables.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "too many Rime syllables"))?;
        let normalized = normalized.into_boxed_str();
        self.syllables.push(normalized.clone());
        self.syllable_ids.insert(normalized, id);
        Ok(Some(id))
    }

    fn add_character(&mut self, ch: char, syllable: SyllableId, weight: u64, overrides: bool) {
        let list = self.char_readings.entry(ch).or_default();
        if let Some((_, current)) = list.iter_mut().find(|(id, _)| *id == syllable) {
            if overrides {
                *current = (*current).max(weight);
            }
        } else {
            list.push((syllable, weight));
        }
    }

    fn add_word(&mut self, word: &str, syllables: &[SyllableId], weight: u64) -> WordId {
        if let Some(&id) = self.word_ids.get(word) {
            let entry = &mut self.words[id as usize];
            if weight > entry.weight {
                self.word_syllables[entry.start..entry.start + entry.len]
                    .copy_from_slice(syllables);
                entry.weight = weight;
            }
            return id;
        }
        let id = as_u32(self.words.len());
        self.word_ids.insert(word.into(), id);
        self.words.push(WordReading {
            start: self.word_syllables.len(),
            len: syllables.len(),
            weight,
        });
        self.word_syllables.extend_from_slice(syllables);
        id
    }

    fn add_homophone(&mut self, reading: &str, word: WordId, weight: u64) {
        if let Some(list) = self.homophones.get_mut(reading) {
            list.push((word, weight));
        } else {
            self.homophones.insert(reading.into(), vec![(word, weight)]);
        }
    }

    fn parse_file(&mut self, path: &Path, default_weight: u64, overrides: bool) -> io::Result<()> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let mut reader = BufReader::new(file);
        let mut in_body = false;
        let mut line = String::new();
        let mut syllables = Vec::new();
        let mut spaced = String::new();

        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let trimmed = line.trim();
            if !in_body {
                in_body = trimmed == "...";
                continue;
            }
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let mut cols = line.split('\t');
            let text = cols.next().unwrap().trim();
            let Some(code) = cols.next().filter(|_| !text.is_empty()) else {
                continue;
            };
            syllables.clear();
            for raw in code.split_whitespace() {
                if let Some(id) = self.syllable_id(raw)? {
                    syllables.push(id);
                }
            }
            let char_count = text.chars().count();
            if syllables.len() != char_count {
                continue;
            }
            let weight = cols
                .next()
                .and_then(|w| w.trim().parse::<u64>().ok())
                .unwrap_or(default_weight)
                .max(1);
            if char_count == 1 {
                self.add_character(
                    text.chars().next().unwrap(),
                    syllables[0],
                    weight,
                    overrides,
                );
            }
            spaced.clear();
            for &id in &syllables {
                if !spaced.is_empty() {
                    spaced.push(' ');
                }
                spaced.push_str(&self.syllables[usize::from(id)]);
            }
            let word = self.add_word(text, &syllables, weight);
            self.add_homophone(&spaced, word, weight);
        }
        Ok(())
    }

    fn finish(self, fingerprint: &Fingerprint) -> RimeDictCorpus {
        let (writer, max_word_chars) = self.into_writer();
        writer.finish(fingerprint, max_word_chars)
    }

    fn into_writer(mut self) -> (CorpusWriter, usize) {
        let mut writer = CorpusWriter::new();
        for syllable in &self.syllables {
            let id = writer.string(syllable);
            writer
                .section(Syllables)
                .extend_from_slice(&id.to_le_bytes());
        }
        let mut characters: Vec<_> = self.char_readings.into_iter().collect();
        characters.sort_unstable_by_key(|(ch, _)| *ch);
        for (ch, mut readings) in characters {
            readings.sort_by_key(|&(_, weight)| Reverse(weight));
            let start = writer.section(CharValues).len() / CHAR_VALUE;
            writer.row(Chars, ch as u32, start, readings.len());
            for (id, weight) in readings {
                let values = writer.section(CharValues);
                values.extend_from_slice(&id.to_le_bytes());
                values.extend_from_slice(&weight.to_le_bytes());
            }
        }

        let mut words: Vec<_> = self
            .word_ids
            .iter()
            .map(|(text, &id)| (text.as_ref(), id))
            .collect();
        words.sort_unstable_by_key(|&(word, _)| word);
        let mut word_strings = vec![0; words.len()];
        let mut bloom = vec![0u64; CORPUS_BLOOM_WORDS];
        let mut max_word_chars = 1;
        let mut abbreviations: HashMap<_, Vec<(WordId, &str, u64)>> = HashMap::new();
        let mut continuations: HashMap<&str, Vec<(&str, u64)>> = HashMap::new();
        for (word, id) in words {
            let entry = &self.words[id as usize];
            let syllables = &self.word_syllables[entry.start..entry.start + entry.len];
            let str_id = writer.string(word);
            word_strings[id as usize] = str_id;
            if syllables.is_empty() {
                continue;
            }
            let start = writer.section(WordSyllables).len() / 2;
            writer.row(Words, str_id, start, entry.len);
            writer
                .section(Words)
                .extend_from_slice(&entry.weight.to_le_bytes());
            for syllable in syllables {
                writer
                    .section(WordSyllables)
                    .extend_from_slice(&syllable.to_le_bytes());
            }

            max_word_chars = max_word_chars.max(entry.len);
            if entry.len <= 5 {
                let mut chars = ['\0'; 5];
                for (target, ch) in chars.iter_mut().zip(word.chars()) {
                    *target = ch;
                }
                let key = pack_word_u128(&chars[..entry.len]).unwrap();
                let (h1, h2) = corpus_bloom_hashes(key);
                for probe in 0..4u64 {
                    let bit = h1.wrapping_add(probe.wrapping_mul(h2)) & (CORPUS_BLOOM_BITS - 1);
                    bloom[(bit >> 6) as usize] |= 1u64 << (bit & 63);
                }
            }
            if (2..=6).contains(&entry.len) {
                let initials = initials(
                    syllables
                        .iter()
                        .map(|&id| self.syllables[usize::from(id)].as_ref()),
                )
                .unwrap();
                insert_ranked(
                    abbreviations.entry(initials).or_default(),
                    16,
                    (id, word, entry.weight),
                    |&(_, text, weight)| (Reverse(weight), text),
                );
                for (i, (offset, _)) in word.char_indices().skip(1).take(3).enumerate() {
                    let (prefix, suffix) = word.split_at(offset);
                    // Both slices borrow the existing word, including on a new bucket.
                    let bucket = continuations.entry(prefix).or_default();
                    insert_ranked(
                        bucket,
                        if i == 0 { 24 } else { 10 },
                        (suffix, entry.weight),
                        |&(text, weight)| (Reverse(weight), text),
                    );
                }
            }
        }
        for word in bloom {
            writer.section(Bloom).extend_from_slice(&word.to_le_bytes());
        }

        let mut homophones: Vec<_> = self.homophones.iter_mut().collect();
        homophones.sort_unstable_by_key(|&(reading, _)| reading.as_ref());
        let mut initial_readings: HashMap<_, Vec<(u32, &str, u64)>> = HashMap::new();
        let mut seen = HashSet::with_capacity(120);
        for (reading, words) in homophones {
            let limit = if reading.contains(' ') { 48 } else { 120 };
            words.sort_by_key(|&(_, weight)| Reverse(weight));
            seen.clear();
            words.retain(|&(word, _)| seen.len() < limit && seen.insert(word));
            let key = writer.string(reading);
            let start = writer.section(ReadingValues).len() / WEIGHTED_VALUE;
            writer.row(Readings, key, start, words.len());
            for &(word, weight) in words.iter() {
                writer.weighted(ReadingValues, word_strings[word as usize], weight);
            }
            if let Some(initials) = initials(reading.split(' ')) {
                let entry = (key, reading.as_ref(), words[0].1);
                insert_ranked(
                    initial_readings.entry(initials).or_default(),
                    128,
                    entry,
                    |&(_, text, weight)| (Reverse(weight), text),
                );
            }
        }

        let mut initial_readings: Vec<_> = initial_readings.into_iter().collect();
        initial_readings.sort_unstable_by_key(|(key, _)| *key);
        for (initials, readings) in initial_readings {
            let key = writer.string(initials_text(&initials));
            let start = writer.section(InitialValues).len() / 4;
            writer.row(Initials, key, start, readings.len());
            for (reading, _, _) in readings {
                writer
                    .section(InitialValues)
                    .extend_from_slice(&reading.to_le_bytes());
            }
        }

        let mut abbreviations: Vec<_> = abbreviations.into_iter().collect();
        abbreviations.sort_unstable_by_key(|(key, _)| *key);
        for (initials, words) in abbreviations {
            let key = writer.string(initials_text(&initials));
            let start = writer.section(AbbreviationValues).len() / WEIGHTED_VALUE;
            writer.row(Abbreviations, key, start, words.len());
            for (word, _, weight) in words {
                writer.weighted(AbbreviationValues, word_strings[word as usize], weight);
            }
        }

        let mut continuations: Vec<_> = continuations.into_iter().collect();
        continuations.sort_unstable_by_key(|&(prefix, _)| prefix);
        for (prefix, suffixes) in continuations {
            let key = writer.string(prefix);
            let start = writer.section(ContinuationValues).len() / WEIGHTED_VALUE;
            writer.row(Continuations, key, start, suffixes.len());
            for (suffix, weight) in suffixes {
                let id = self.word_ids.get(suffix).map_or_else(
                    || writer.string(suffix),
                    |&word| word_strings[word as usize],
                );
                writer.weighted(ContinuationValues, id, weight);
            }
        }
        (writer, max_word_chars.min(12))
    }

    fn seed_core_fallback(&mut self) {
        const FALLBACK_CHARS: &[(&str, &str, u64)] = &[
            ("你", "ni", 10_000_000),
            ("好", "hao", 10_000_000),
            ("世", "shi", 5_000_000),
            ("界", "jie", 5_000_000),
            ("我", "wo", 20_000_000),
            ("女", "nv", 5_000_000),
            ("绿", "lv", 3_000_000),
            ("旅", "lv", 2_000_000),
            ("虐", "nve", 500_000),
            ("略", "lve", 1_000_000),
            ("局", "ju", 3_000_000),
            ("需", "xu", 3_000_000),
            ("握", "wo", 500_000),
            ("为", "wei", 10_000_000),
            ("无", "wu", 8_000_000),
            ("要", "yao", 10_000_000),
            ("打", "da", 8_000_000),
            ("卡", "ka", 5_000_000),
            ("去", "qu", 8_000_000),
            ("的", "de", 76_000_000),
            ("是", "shi", 30_000_000),
            ("这", "zhe", 15_000_000),
            ("个", "ge", 15_000_000),
            ("很", "hen", 8_000_000),
            ("宽", "kuan", 1_000_000),
            ("重", "zhong", 1_900_814),
            ("重", "chong", 114_049),
            ("行", "xing", 2_756_706),
            ("行", "hang", 2_756_706),
            ("长", "chang", 3_100_060),
            ("长", "zhang", 3_100_060),
            ("乐", "le", 2_000_000),
            ("乐", "yue", 1_000_000),
            ("得", "de", 7_186_260),
            ("得", "dei", 7_186_260),
            ("发", "fa", 8_000_000),
            ("耄", "mao", 713),
            ("頔", "di", 10),
            ("先", "xian", 3_000_000),
            ("现", "xian", 3_000_000),
            ("将", "jiang", 3_000_000),
            ("看", "kan", 3_000_000),
            ("在", "zai", 10_000_000),
            ("一", "yi", 20_000_000),
        ];
        for &(ch_s, syl, w) in FALLBACK_CHARS {
            let ch = ch_s.chars().next().unwrap();
            let syllable = self.syllable_id(syl).unwrap().unwrap();
            self.add_character(ch, syllable, w, false);
            // Fallback characters are homophones but are absent from word lookup.
            let id = *self.word_ids.entry(ch_s.into()).or_insert_with(|| {
                let id = as_u32(self.words.len());
                self.words.push(WordReading {
                    start: 0,
                    len: 0,
                    weight: 0,
                });
                id
            });
            self.add_homophone(syl, id, w);
        }
        const FALLBACK_WORDS: &[(&str, &str, u64)] = &[
            ("你好", "ni hao", 1_000_000),
            ("我的", "wo de", 1_000_000),
            ("我要", "wo yao", 900_000),
            ("打卡", "da ka", 500_000),
            ("输入法", "shu ru fa", 500_000),
            ("正确", "zheng que", 500_000),
            ("人工智能", "ren gong zhi neng", 500_000),
            ("没关系", "mei guan xi", 500_000),
            ("为什么", "wei shen me", 500_000),
            ("不知道", "bu zhi dao", 500_000),
            ("怎么样", "zen me yang", 500_000),
            ("世界", "shi jie", 1_000_000),
            ("你好世界", "ni hao shi jie", 1_000_000),
            ("西安", "xi an", 55_003),
            ("旅游", "lv you", 500_977),
            ("兵马俑", "bing ma yong", 21_060),
            ("方案", "fang an", 500_755),
            ("反感", "fan gan", 76_460),
            ("让人", "rang ren", 500_554),
            ("决定", "jue ding", 501_496),
            ("时间", "shi jian", 504_677),
            ("实践", "shi jian", 500_546),
            ("出真知", "chu zhen zhi", 50_000),
            ("真理", "zhen li", 210_475),
            ("情绪", "qing xu", 500_335),
            ("激昂", "ji ang", 23_740),
            ("大家", "da jia", 501_724),
            ("奋发", "fen fa", 15_120),
            ("非常", "fei chang", 501_000),
            ("艰难", "jian nan", 136_480),
            ("建安", "jian an", 37_385),
            ("北京", "bei jing", 1_119_506),
            ("长安街", "chang an jie", 15_315),
            ("长安", "chang an", 120_000),
            ("平安", "ping an", 304_880),
            ("一路", "yi lu", 455_090),
            ("祝你", "zhu ni", 72_860),
            ("顺利", "shun li", 481_230),
            ("重要", "zhong yao", 500_940),
            ("重复", "chong fu", 500_291),
            ("银行", "yin hang", 500_769),
            ("营业", "ying ye", 400_000),
            ("自由", "zi you", 500_000),
            ("行走", "xing zou", 184_425),
            ("茁壮", "zhuo zhuang", 100_000),
            ("成长", "cheng zhang", 500_392),
            ("测量", "ce liang", 300_000),
            ("长度", "chang du", 400_000),
            ("快乐", "kuai le", 500_000),
            ("生活", "sheng huo", 600_000),
            ("古典", "gu dian", 300_000),
            ("音乐", "yin yue", 500_000),
            ("得到", "de dao", 501_238),
            ("认可", "ren ke", 400_000),
            ("非得", "fei dei", 46_060),
            ("如此", "ru ci", 500_000),
            ("觉得", "jue de", 500_907),
            ("满意", "man yi", 500_000),
            ("发现", "fa xian", 501_897),
            ("问题", "wen ti", 800_000),
            ("黑色", "hei se", 400_000),
            ("头发", "tou fa", 455_135),
            ("曝光", "bao guang", 167_850),
            ("老登", "lao deng", 111),
            ("金币", "jin bi", 501_098),
        ];
        for &(w, py, freq) in FALLBACK_WORDS {
            let syllables: Vec<_> = py
                .split_whitespace()
                .map(|s| self.syllable_id(s).unwrap().unwrap())
                .collect();
            let word = self.add_word(w, &syllables, freq);
            self.add_homophone(py, word, freq);
        }
    }
}

fn initials(syllables: impl Iterator<Item = impl AsRef<str>>) -> Option<[u8; 6]> {
    let mut key = [0; 6];
    let mut len = 0;
    for syllable in syllables {
        *key.get_mut(len)? = syllable.as_ref().as_bytes()[0];
        len += 1;
    }
    (len >= 2).then_some(key)
}

fn initials_text(key: &[u8; 6]) -> &str {
    std::str::from_utf8(key).unwrap().trim_end_matches('\0')
}

fn insert_ranked<T, K: Ord>(bucket: &mut Vec<T>, limit: usize, value: T, key: impl Fn(&T) -> K) {
    let value_key = key(&value);
    let rank = bucket.partition_point(|entry| key(entry) < value_key);
    if rank < limit {
        bucket.insert(rank, value);
        bucket.truncate(limit);
    }
}

/// Curated toned-word override table resolving homophonic & polyphonic tone ambiguity
/// (`发现` `fā` vs `头发` `fà`, `得到` `dé` vs `觉得` `de`, `的确` `dí` vs `目的` `dì`, etc.).
fn toned_word_overrides() -> &'static HashMap<&'static str, &'static [&'static str]> {
    static MAP: OnceLock<HashMap<&'static str, &'static [&'static str]>> = OnceLock::new();
    MAP.get_or_init(|| {
        let pairs: &[(&str, &[&str])] = &[
            // 发 fā / fà
            ("发现", &["fā", "xiàn"]),
            ("发展", &["fā", "zhǎn"]),
            ("发生", &["fā", "shēng"]),
            ("发明", &["fā", "míng"]),
            ("发表", &["fā", "biǎo"]),
            ("发送", &["fā", "sòng"]),
            ("发出", &["fā", "chū"]),
            ("发布", &["fā", "bù"]),
            ("发财", &["fā", "cái"]),
            ("出发", &["chū", "fā"]),
            ("爆发", &["bào", "fā"]),
            ("研发", &["yán", "fā"]),
            ("头发", &["tóu", "fà"]),
            ("白发", &["bái", "fà"]),
            ("黑发", &["hēi", "fà"]),
            ("毛发", &["máo", "fà"]),
            ("理发", &["lǐ", "fà"]),
            ("美发", &["měi", "fà"]),
            ("假发", &["jiǎ", "fà"]),
            ("短发", &["duǎn", "fà"]),
            ("长发", &["cháng", "fà"]),
            ("秀发", &["xiù", "fà"]),
            ("发丝", &["fà", "sī"]),
            ("发型", &["fà", "xíng"]),
            ("发挥", &["fā", "huī"]),
            ("发起", &["fā", "qǐ"]),
            ("发热", &["fā", "rè"]),
            ("发票", &["fā", "piào"]),
            ("发烧", &["fā", "shāo"]),
            ("发廊", &["fà", "láng"]),
            ("脱发", &["tuō", "fà"]),
            ("生发", &["shēng", "fà"]),
            // 得 dé / de / děi
            ("得到", &["dé", "dào"]),
            ("获得", &["huò", "dé"]),
            ("取得", &["qǔ", "dé"]),
            ("赢得", &["yíng", "dé"]),
            ("博得", &["bó", "dé"]),
            ("求得", &["qiú", "dé"]),
            ("拾得", &["shí", "dé"]),
            ("测得", &["cè", "dé"]),
            ("夺得", &["duó", "dé"]),
            ("争得", &["zhēng", "dé"]),
            ("值得", &["zhí", "de"]),
            ("觉得", &["jué", "de"]),
            ("记得", &["jì", "de"]),
            ("懂得", &["dǒng", "de"]),
            ("晓得", &["xiǎo", "de"]),
            ("舍得", &["shě", "de"]),
            ("免得", &["miǎn", "de"]),
            ("省得", &["shěng", "de"]),
            ("懒得", &["lǎn", "de"]),
            ("难得", &["nán", "dé"]),
            ("心得", &["xīn", "dé"]),
            ("自得", &["zì", "dé"]),
            ("得意", &["dé", "yì"]),
            ("得体", &["dé", "tǐ"]),
            ("得当", &["dé", "dàng"]),
            ("得力", &["dé", "lì"]),
            ("得法", &["dé", "fǎ"]),
            ("得分", &["dé", "fēn"]),
            ("得奖", &["dé", "jiǎng"]),
            ("得罪", &["dé", "zuì"]),
            ("得以", &["dé", "yǐ"]),
            ("得悉", &["dé", "xī"]),
            ("得知", &["dé", "zhī"]),
            ("得失", &["dé", "shī"]),
            ("得手", &["dé", "shǒu"]),
            ("得胜", &["dé", "shèng"]),
            ("得救", &["dé", "jiù"]),
            ("得寸进尺", &["dé", "cùn", "jìn", "chǐ"]),
            ("得心应手", &["dé", "xīn", "yìng", "shǒu"]),
            ("非得", &["fēi", "děi"]),
            ("总得", &["zǒng", "děi"]),
            ("必得", &["bì", "děi"]),
            ("就得", &["jiù", "děi"]),
            // 兴 xìng / xīng
            ("高兴", &["gāo", "xìng"]),
            ("兴趣", &["xìng", "qù"]),
            ("兴致", &["xìng", "zhì"]),
            ("扫兴", &["sǎo", "xìng"]),
            ("尽兴", &["jìn", "xìng"]),
            ("乘兴", &["chéng", "xìng"]),
            ("助兴", &["zhù", "xìng"]),
            ("败兴", &["bài", "xìng"]),
            ("雅兴", &["yǎ", "xìng"]),
            ("即兴", &["jí", "xìng"]),
            ("兴奋", &["xīng", "fèn"]),
            ("兴旺", &["xīng", "wàng"]),
            ("兴盛", &["xīng", "shèng"]),
            ("兴起", &["xīng", "qǐ"]),
            ("兴建", &["xīng", "jiàn"]),
            ("复兴", &["fù", "xīng"]),
            ("新兴", &["xīn", "xīng"]),
            ("振兴", &["zhèn", "xīng"]),
            // 重 zhòng / chóng
            ("重要", &["zhòng", "yào"]),
            ("重点", &["zhòng", "diǎn"]),
            ("重大", &["zhòng", "dà"]),
            ("重量", &["zhòng", "liàng"]),
            ("重力", &["zhòng", "lì"]),
            ("重视", &["zhòng", "shì"]),
            ("严重", &["yán", "zhòng"]),
            ("尊重", &["zūn", "zhòng"]),
            ("沉重", &["chén", "zhòng"]),
            ("体重", &["tǐ", "zhòng"]),
            ("重复", &["chóng", "fù"]),
            ("重新", &["chóng", "xīn"]),
            ("重建", &["chóng", "jiàn"]),
            ("重逢", &["chóng", "féng"]),
            ("重叠", &["chóng", "dié"]),
            ("双重", &["shuāng", "chóng"]),
            ("多重", &["duō", "chóng"]),
            ("重温", &["chóng", "wēn"]),
            ("重现", &["chóng", "xiàn"]),
            ("重排", &["chóng", "pái"]),
            ("语义重排", &["yǔ", "yì", "chóng", "pái"]),
            // 行 háng / xíng
            ("银行", &["yín", "háng"]),
            ("行业", &["háng", "yè"]),
            ("同行", &["tóng", "háng"]),
            ("行列", &["háng", "liè"]),
            ("内行", &["nèi", "háng"]),
            ("外行", &["wài", "háng"]),
            ("行家", &["háng", "jiā"]),
            ("排行", &["pái", "háng"]),
            ("发行", &["fā", "xíng"]),
            ("行走", &["xíng", "zǒu"]),
            ("行动", &["xíng", "dòng"]),
            ("行为", &["xíng", "wéi"]),
            ("进行", &["jìn", "xíng"]),
            ("执行", &["zhí", "xíng"]),
            ("运行", &["yùn", "xíng"]),
            ("流行", &["liú", "xíng"]),
            ("旅行", &["lǚ", "xíng"]),
            ("不行", &["bù", "xíng"]),
            // 长 zhǎng / cháng
            ("成长", &["chéng", "zhǎng"]),
            ("长大", &["zhǎng", "dà"]),
            ("生长", &["shēng", "zhǎng"]),
            ("家长", &["jiā", "zhǎng"]),
            ("校长", &["xiào", "zhǎng"]),
            ("部长", &["bù", "zhǎng"]),
            ("院长", &["yuàn", "zhǎng"]),
            ("长度", &["cháng", "dù"]),
            ("长短", &["cháng", "duǎn"]),
            ("长期", &["cháng", "qī"]),
            ("长城", &["cháng", "chéng"]),
            ("漫长", &["màn", "cháng"]),
            ("擅长", &["shàn", "cháng"]),
            ("特长", &["tè", "cháng"]),
            // 乐 lè / yuè
            ("快乐", &["kuài", "lè"]),
            ("欢乐", &["huān", "lè"]),
            ("乐观", &["lè", "guān"]),
            ("乐趣", &["lè", "qù"]),
            ("可乐", &["kě", "lè"]),
            ("娱乐", &["yú", "lè"]),
            ("音乐", &["yīn", "yuè"]),
            ("乐器", &["yuè", "qì"]),
            ("乐队", &["yuè", "duì"]),
            ("乐章", &["yuè", "zhāng"]),
            ("奏乐", &["zòu", "yuè"]),
            // 的 / 地 / 好 / 数 / 切 / 为 / 处 / 看 / 量 / 觉 / 还 / 相 / 应 / 差 / 会 / 分 / 干 / 空 / 种 / 藏 / 转 / 都 / 朝
            ("目的", &["mù", "dì"]),
            ("的确", &["dí", "què"]),
            ("打的", &["dǎ", "dī"]),
            ("的士", &["dī", "shì"]),
            ("面的", &["miàn", "dī"]),
            ("摩的", &["mó", "dī"]),
            ("地地道道", &["dì", "dì", "dào", "dào"]),
            ("地道", &["dì", "dào"]),
            ("土地", &["tǔ", "dì"]),
            ("地方", &["dì", "fāng"]),
            ("地球", &["dì", "qiú"]),
            ("地铁", &["dì", "tiě"]),
            ("地面", &["dì", "miàn"]),
            ("地下", &["dì", "xià"]),
            ("地点", &["dì", "diǎn"]),
            ("地区", &["dì", "qū"]),
            ("地址", &["dì", "zhǐ"]),
            ("地图", &["dì", "tú"]),
            ("爱好", &["ài", "hào"]),
            ("好奇", &["hào", "qí"]),
            ("好客", &["hào", "kè"]),
            ("好学", &["hào", "xué"]),
            ("好胜", &["hào", "shèng"]),
            ("好强", &["hào", "qiáng"]),
            ("好战", &["hào", "zhàn"]),
            ("好斗", &["hào", "dòu"]),
            ("喜好", &["xǐ", "hào"]),
            ("嗜好", &["shì", "hào"]),
            ("美好", &["měi", "hǎo"]),
            ("友好", &["yǒu", "hǎo"]),
            ("良好", &["liáng", "hǎo"]),
            ("满意", &["mǎn", "yì"]),
            ("认可", &["rèn", "kě"]),
            ("数学", &["shù", "xué"]),
            ("数字", &["shù", "zì"]),
            ("数据", &["shù", "jù"]),
            ("数量", &["shù", "liàng"]),
            ("倒数", &["dào", "shǔ"]),
            ("一切", &["yī", "qiè"]),
            ("亲切", &["qīn", "qiè"]),
            ("密切", &["mì", "qiè"]),
            ("迫切", &["pò", "qiè"]),
            ("急切", &["jí", "qiè"]),
            ("恳切", &["kěn", "qiè"]),
            ("确切", &["què", "qiè"]),
            ("切记", &["qiè", "jì"]),
            ("切勿", &["qiè", "wù"]),
            ("切忌", &["qiè", "jì"]),
            ("切实", &["qiè", "shí"]),
            ("关切", &["guān", "qiè"]),
            ("切换", &["qiē", "huàn"]),
            ("切割", &["qiē", "gē"]),
            ("切菜", &["qiē", "cài"]),
            ("切肉", &["qiē", "ròu"]),
            ("切开", &["qiē", "kāi"]),
            ("切断", &["qiē", "duàn"]),
            ("切片", &["qiē", "piàn"]),
            ("切块", &["qiē", "kuài"]),
            ("切丝", &["qiē", "sī"]),
            ("切碎", &["qiē", "suì"]),
            ("切除", &["qiē", "chú"]),
            ("切磋", &["qiē", "cuō"]),
            ("切线", &["qiē", "xiàn"]),
            ("教育", &["jiào", "yù"]),
            ("教师", &["jiào", "shī"]),
            ("教学", &["jiào", "xué"]),
            ("教授", &["jiào", "shòu"]),
            ("教材", &["jiào", "cái"]),
            ("教练", &["jiào", "liàn"]),
            ("教训", &["jiào", "xùn"]),
            ("教书", &["jiāo", "shū"]),
            ("着急", &["zháo", "jí"]),
            ("着火", &["zháo", "huǒ"]),
            ("着凉", &["zháo", "liáng"]),
            ("睡着", &["shuì", "zháo"]),
            ("着迷", &["zháo", "mí"]),
            ("执着", &["zhí", "zhuó"]),
            ("沉着", &["chén", "zhuó"]),
            ("着手", &["zhuó", "shǒu"]),
            ("着陆", &["zhuó", "lù"]),
            ("着想", &["zhuó", "xiǎng"]),
            ("传统", &["chuán", "tǒng"]),
            ("传递", &["chuán", "dì"]),
            ("传播", &["chuán", "bō"]),
            ("自传", &["zì", "zhuàn"]),
            ("传记", &["zhuàn", "jì"]),
            ("模型", &["mó", "xíng"]),
            ("模式", &["mó", "shì"]),
            ("模糊", &["mó", "hu"]),
            ("模拟", &["mó", "nǐ"]),
            ("规模", &["guī", "mó"]),
            ("模样", &["mú", "yàng"]),
            ("模具", &["mú", "jù"]),
            ("弹琴", &["tán", "qín"]),
            ("弹簧", &["tán", "huáng"]),
            ("反弹", &["fǎn", "tán"]),
            ("弹性", &["tán", "xìng"]),
            ("子弹", &["zǐ", "dàn"]),
            ("炸弹", &["zhà", "dàn"]),
            ("导弹", &["dǎo", "dàn"]),
            ("困难", &["kùn", "nán"]),
            ("难题", &["nán", "tí"]),
            ("灾难", &["zāi", "nàn"]),
            ("遇难", &["yù", "nàn"]),
            ("避难", &["bì", "nàn"]),
            ("难民", &["nàn", "mín"]),
            ("宿舍", &["sù", "shè"]),
            ("住宿", &["zhù", "sù"]),
            ("一宿", &["yī", "xiǔ"]),
            ("星宿", &["xīng", "xiù"]),
            ("银行行长", &["yín", "háng", "háng", "zhǎng"]),
            ("之道", &["zhī", "dào"]),
            ("回家", &["huí", "jiā"]),
            ("做饭", &["zuò", "fàn"]),
            ("因为", &["yīn", "wèi"]),
            ("为了", &["wèi", "le"]),
            ("作为", &["zuò", "wéi"]),
            ("成为", &["chéng", "wéi"]),
            ("认为", &["rèn", "wéi"]),
            ("处理", &["chǔ", "lǐ"]),
            ("相处", &["xiāng", "chǔ"]),
            ("到处", &["dào", "chù"]),
            ("好处", &["hǎo", "chù"]),
            ("看守", &["kān", "shǒu"]),
            ("看护", &["kān", "hù"]),
            ("看见", &["kàn", "jiàn"]),
            ("看法", &["kàn", "fǎ"]),
            ("测量", &["cè", "liáng"]),
            ("力量", &["lì", "liàng"]),
            ("能量", &["néng", "liàng"]),
            ("衡量", &["héng", "liáng"]),
            ("睡觉", &["shuì", "jiào"]),
            ("感觉", &["gǎn", "jué"]),
            ("还是", &["hái", "shì"]),
            ("归还", &["guī", "huán"]),
            ("还原", &["huán", "yuán"]),
            ("相同", &["xiāng", "tóng"]),
            ("相信", &["xiāng", "xìn"]),
            ("照片", &["zhào", "piàn"]),
            ("相片", &["xiàng", "piàn"]),
            ("长相", &["zhǎng", "xiàng"]),
            ("应该", &["yīng", "gāi"]),
            ("应用", &["yìng", "yòng"]),
            ("反应", &["fǎn", "yìng"]),
            ("适应", &["shì", "yìng"]),
            ("差距", &["chā", "jù"]),
            ("差别", &["chā", "bié"]),
            ("出差", &["chū", "chāi"]),
            ("会计", &["kuài", "jì"]),
            ("会议", &["huì", "yì"]),
            ("机会", &["jī", "huì"]),
            ("分别", &["fēn", "bié"]),
            ("部分", &["bù", "fen"]),
            ("成分", &["chéng", "fèn"]),
            ("干净", &["gān", "jìng"]),
            ("干燥", &["gān", "zào"]),
            ("干部", &["gàn", "bù"]),
            ("空间", &["kōng", "jiān"]),
            ("空气", &["kōng", "qì"]),
            ("空白", &["kòng", "bái"]),
            ("种子", &["zhǒng", "zi"]),
            ("种类", &["zhǒng", "lèi"]),
            ("种植", &["zhòng", "zhí"]),
            ("隐藏", &["yǐn", "cáng"]),
            ("收藏", &["shōu", "cáng"]),
            ("西藏", &["xī", "zàng"]),
            ("宝藏", &["bǎo", "zàng"]),
            ("旋转", &["xuán", "zhuǎn"]),
            ("转动", &["zhuàn", "dòng"]),
            ("首都", &["shǒu", "dū"]),
            ("都市", &["dū", "shì"]),
            ("都是", &["dōu", "shì"]),
            ("朝阳", &["zhāo", "yáng"]),
            ("朝代", &["cháo", "dài"]),
            ("曝光", &["bào", "guāng"]),
            ("爆光", &["bào", "guāng"]),
            ("老登", &["lǎo", "dēng"]),
            ("爆金币", &["bào", "jīn", "bì"]),
            ("老登金币", &["lǎo", "dēng", "jīn", "bì"]),
            ("金币", &["jīn", "bì"]),
            ("晶透", &["jīng", "tòu"]),
            ("毛玻璃", &["máo", "bō", "li"]),
            ("雾面玻璃", &["wù", "miàn", "bō", "li"]),
            ("小鹤双拼", &["xiǎo", "hè", "shuāng", "pīn"]),
            ("全拼解码", &["quán", "pīn", "jiě", "mǎ"]),
            ("剪贴板", &["jiǎn", "tiē", "bǎn"]),
            ("离线语音", &["lí", "xiàn", "yǔ", "yīn"]),
            ("连续手写", &["lián", "xù", "shǒu", "xiě"]),
            ("端到端测试", &["duān", "dào", "duān", "cè", "shì"]),
        ];
        pairs.iter().copied().collect()
    })
}

pub struct PolyphoneAnnotator {
    corpus: Arc<RimeDictCorpus>,
}

#[derive(Debug, Clone)]
pub struct AnnotatedPinyin {
    pub untoned: String,
    pub toned: String,
    pub xiaohe: String,
}

impl PolyphoneAnnotator {
    pub fn new(corpus: Arc<RimeDictCorpus>) -> Self {
        Self { corpus }
    }

    pub fn from_assets_dir(assets_dir: Option<&Path>) -> Self {
        Self::new(RimeDictCorpus::load_or_fallback(assets_dir))
    }

    pub fn corpus(&self) -> &Arc<RimeDictCorpus> {
        &self.corpus
    }

    /// Return deduplicated `(toned_pinyin, xiaohe_code)` pairs for a single Chinese character.
    /// Combines `8105` / `41448` reading priority with `pinyin = "0.11"` heteronyms.
    pub fn char_phonetics(&self, ch: char) -> Vec<(String, String)> {
        let mut toned_candidates: Vec<String> = Vec::new();
        if let Some(multi) = ch.to_pinyin_multi() {
            for p in multi {
                let t = p.with_tone();
                if !t.is_empty() && !toned_candidates.iter().any(|x| x == t) {
                    toned_candidates.push(t.to_string());
                }
            }
        } else if let Some(p) = ch.to_pinyin() {
            let t = p.with_tone();
            if !t.is_empty() {
                toned_candidates.push(t.to_string());
            }
        }

        let mut results: Vec<(String, String)> = Vec::new();
        let mut seen_toned: HashSet<String> = HashSet::new();

        for (untoned, _) in self.corpus.char_readings(ch) {
            // Find matching toned variant from `pinyin` crate
            let mut matched = false;
            for toned in &toned_candidates {
                if normalize_syllable(toned) == untoned && seen_toned.insert(toned.clone()) {
                    let xh = syllable_to_xiaohe(toned);
                    results.push((toned.clone(), xh));
                    matched = true;
                }
            }
            if !matched && toned_candidates.is_empty() && seen_toned.insert(untoned.to_owned()) {
                let xh = syllable_to_xiaohe(untoned);
                results.push((untoned.to_owned(), xh));
            }
        }

        for toned in toned_candidates {
            if seen_toned.insert(toned.clone()) {
                let xh = syllable_to_xiaohe(&toned);
                results.push((toned, xh));
            }
        }

        results
    }

    /// Stage 1 + Stage 2 polyphone auto-pinyin for un-annotated Chinese text.
    pub fn annotate_text(&self, text: &str) -> AnnotatedPinyin {
        let chars: Vec<char> = text.chars().collect();
        let n = chars.len();
        if n == 0 {
            return AnnotatedPinyin {
                untoned: String::new(),
                toned: String::new(),
                xiaohe: String::new(),
            };
        }

        // Stage 1: Viterbi word-lattice segmentation over rime-ice (base + ext + 8105 + 41448 + others)
        let mut dp_score = vec![f64::NEG_INFINITY; n + 1];
        let mut dp_prev: Vec<Option<(usize, Vec<String>)>> = vec![None; n + 1];
        dp_score[0] = 0.0;

        let max_len = self.corpus.max_word_chars().min(n).max(4.min(n));
        let overrides = toned_word_overrides();

        for i in 0..n {
            if !dp_score[i].is_finite() {
                continue;
            }
            // Multi-char words from lexicon & curated polyphone overrides
            for len in 2..=max_len {
                if i + len > n {
                    break;
                }
                let span: String = chars[i..i + len].iter().collect();
                if let Some(toned_slice) = overrides.get(span.as_str()) {
                    if toned_slice.len() == len {
                        let syls: Vec<String> =
                            toned_slice.iter().map(|s| normalize_syllable(s)).collect();
                        let corpus_w = self
                            .corpus
                            .word_weight(&span)
                            .unwrap_or(200_000)
                            .max(200_000);
                        let edge_score = ((corpus_w as f64) + 1.0).ln()
                            + (len as f64) * 14.0
                            + ((len - 1) as f64) * 4.0;
                        let cand = dp_score[i] + edge_score;
                        if cand > dp_score[i + len] {
                            dp_score[i + len] = cand;
                            dp_prev[i + len] = Some((i, syls));
                        }
                        continue;
                    }
                }
                if let Some((syls, weight)) = self.corpus.word_reading(&span) {
                    let edge_score = ((weight as f64) + 1.0).ln()
                        + (len as f64) * 14.0
                        + ((len - 1) as f64) * 4.0;
                    let cand = dp_score[i] + edge_score;
                    if cand > dp_score[i + len] {
                        dp_score[i + len] = cand;
                        dp_prev[i + len] = Some((i, syls.map(str::to_owned).collect()));
                    }
                }
            }

            // Single-char fallback (with contextual neighbor rules for OOV boundaries)
            let ch = chars[i];
            let syl = self.single_char_untoned_with_context(&chars, i);
            let char_w = self
                .corpus
                .char_readings(ch)
                .next()
                .map(|(_, weight)| weight)
                .unwrap_or(100);
            let single_score = ((char_w as f64) + 1.0).ln() * 0.75;
            let cand = dp_score[i] + single_score;
            if cand > dp_score[i + 1] {
                dp_score[i + 1] = cand;
                dp_prev[i + 1] = Some((i, vec![syl]));
            }
        }

        // Backtrack Stage 1 segments
        let mut segments: Vec<(usize, usize, Vec<String>)> = Vec::new();
        let mut cur = n;
        while cur > 0 {
            if let Some((prev, syls)) = &dp_prev[cur] {
                segments.push((*prev, cur, syls.clone()));
                cur = *prev;
            } else {
                let syl = self.single_char_untoned_with_context(&chars, cur - 1);
                segments.push((cur - 1, cur, vec![syl]));
                cur -= 1;
            }
        }
        segments.reverse();

        let mut untoned_syls: Vec<String> = Vec::with_capacity(n);
        for (_, _, syls) in &segments {
            untoned_syls.extend(syls.iter().cloned());
        }

        // Stage 2: Tone disambiguation via toned overrides + particle rules + heteronym filter
        let toned_syls = self.resolve_tones(&chars, &segments, &untoned_syls);

        let xiaohe: String = untoned_syls.iter().map(|s| syllable_to_xiaohe(s)).collect();
        AnnotatedPinyin {
            untoned: untoned_syls.join(" "),
            toned: toned_syls.join(" "),
            xiaohe,
        }
    }

    /// Stage 2 only when untoned syllables are already provided (e.g. `.scel`, `.dict.yaml`, `.tsv`).
    pub fn annotate_with_untoned_hint(
        &self,
        text: &str,
        untoned_hint: &[String],
    ) -> AnnotatedPinyin {
        let chars: Vec<char> = text.chars().collect();
        if chars.len() != untoned_hint.len() || chars.is_empty() {
            let untoned: Vec<String> = untoned_hint
                .iter()
                .map(|s| normalize_syllable(s))
                .filter(|s| !s.is_empty())
                .collect();
            let xiaohe: String = untoned.iter().map(|s| syllable_to_xiaohe(s)).collect();
            return AnnotatedPinyin {
                untoned: untoned.join(" "),
                toned: untoned.join(" "),
                xiaohe,
            };
        }

        let norm_untoned: Vec<String> = untoned_hint
            .iter()
            .map(|s| normalize_syllable(s))
            .collect();
        let segments = vec![(0usize, chars.len(), norm_untoned.clone())];
        let toned_syls = self.resolve_tones(&chars, &segments, &norm_untoned);
        let xiaohe: String = norm_untoned.iter().map(|s| syllable_to_xiaohe(s)).collect();
        AnnotatedPinyin {
            untoned: norm_untoned.join(" "),
            toned: toned_syls.join(" "),
            xiaohe,
        }
    }

    fn single_char_untoned_with_context(&self, chars: &[char], idx: usize) -> String {
        let ch = chars[idx];
        let prev = idx.checked_sub(1).map(|i| chars[i]);
        let next = chars.get(idx + 1).copied();

        match ch {
            '的' => {
                if matches!(prev, Some('打' | '面' | '摩')) || matches!(next, Some('士')) {
                    return "di".to_string();
                }
                if matches!(prev, Some('目'))
                    || (matches!(prev, Some('有')) && matches!(next, Some('放')))
                {
                    return "di".to_string();
                }
            }
            '得' => {
                if matches!(prev, Some('非' | '必' | '总' | '就' | '还' | '也')) {
                    return "dei".to_string();
                }
                return "de".to_string();
            }
            '行' => {
                if matches!(prev, Some('银' | '同' | '排' | '内' | '外'))
                    || matches!(next, Some('业' | '列' | '家'))
                {
                    return "hang".to_string();
                }
            }
            '重' => {
                if matches!(next, Some('复' | '新' | '建' | '逢' | '叠' | '温' | '现')) {
                    return "chong".to_string();
                }
            }
            '长' => {
                if matches!(prev, Some('成' | '生' | '家' | '校' | '部' | '院' | '市' | '局'))
                    || matches!(next, Some('大' | '辈'))
                {
                    return "zhang".to_string();
                }
            }
            '乐'
                if matches!(prev, Some('音' | '奏' | '管' | '民' | '交'))
                    || matches!(next, Some('器' | '队' | '章' | '曲' | '团')) =>
            {
                return "yue".to_string();
            }
            _ => {}
        }

        if let Some(first) = self
            .corpus
            .char_readings(ch)
            .next()
            .map(|(syllable, _)| syllable.to_owned())
        {
            return first;
        }
        if let Some(p) = ch.to_pinyin() {
            return normalize_syllable(p.plain());
        }
        String::new()
    }

    fn resolve_tones(
        &self,
        chars: &[char],
        segments: &[(usize, usize, Vec<String>)],
        untoned_syls: &[String],
    ) -> Vec<String> {
        let n = chars.len();
        let mut out: Vec<Option<String>> = vec![None; n];
        let overrides = toned_word_overrides();

        // 2a. Apply exact toned overrides on matched segments first, then sliding windows (length 4..=2)
        for &(start, end, _) in segments {
            if end > start + 1 {
                let word: String = chars[start..end].iter().collect();
                if let Some(toned_slice) = overrides.get(word.as_str()) {
                    if toned_slice.len() == end - start {
                        for (k, &t) in toned_slice.iter().enumerate() {
                            if normalize_syllable(t) == untoned_syls[start + k] {
                                out[start + k] = Some(t.to_string());
                            }
                        }
                    }
                }
            }
        }

        for win in (2..=4usize).rev() {
            if n < win {
                continue;
            }
            for start in 0..=(n - win) {
                if (start..start + win).all(|k| out[k].is_some()) {
                    continue;
                }
                let word: String = chars[start..start + win].iter().collect();
                if let Some(toned_slice) = overrides.get(word.as_str()) {
                    if toned_slice.len() == win {
                        for (k, &t) in toned_slice.iter().enumerate() {
                            if out[start + k].is_none()
                                && normalize_syllable(t) == untoned_syls[start + k]
                            {
                                out[start + k] = Some(t.to_string());
                            }
                        }
                    }
                }
            }
        }

        // 2b. Grammatical particle & heteronym filtering via `pinyin` crate
        let mut resolved = Vec::with_capacity(n);
        for i in 0..n {
            if let Some(t) = out[i].take() {
                resolved.push(t);
                continue;
            }
            let ch = chars[i];
            let target = &untoned_syls[i];
            let prev = i.checked_sub(1).map(|p| chars[p]);
            let next = chars.get(i + 1).copied();

            // Grammatical neutral-tone particles & contextual heteronym rules
            let is_de_complement = prev.is_some()
                && !matches!(
                    prev,
                    Some('获' | '取' | '赢' | '博' | '求' | '拾' | '测' | '夺' | '争' | '心' | '自' | '可' | '难')
                )
                && matches!(
                    next,
                    Some(
                        '很' | '太' | '真' | '好' | '坏' | '快' | '慢' | '高' | '低' | '大'
                            | '小' | '多' | '少' | '早' | '晚' | '远' | '近' | '清' | '明'
                            | '起' | '住' | '下' | '出' | '来' | '去' | '开' | '稳' | '准'
                            | '香' | '甜' | '苦' | '累' | '惨' | '棒' | '对' | '错' | '行'
                            | '着' | '够' | '更' | '越' | '最' | '挺' | '蛮' | '极' | '十'
                            | '像' | '似' | '不' | '要' | '死' | '完' | '懂' | '见' | '到'
                            | '了' | '上' | '过' | '掉' | '成' | '漂' | '干' | '整' | '响'
                            | '亮' | '久' | '深' | '浅' | '重' | '轻' | '紧' | '松' | '痛'
                            | '顺' | '热' | '冷'
                    )
                );

            let particle_override = match (ch, target.as_str()) {
                ('的', "de") => Some("de"),
                ('的', "di")
                    if matches!(prev, Some('打' | '面' | '摩')) || matches!(next, Some('士')) =>
                {
                    Some("dī")
                }
                ('得', "de")
                    if matches!(
                        prev,
                        Some('觉' | '记' | '懂' | '晓' | '舍' | '值' | '免' | '省' | '懒')
                    ) || is_de_complement =>
                {
                    Some("de")
                }
                ('切', "qie")
                    if matches!(
                        next,
                        Some(
                            '菜' | '肉' | '果' | '瓜' | '片' | '块' | '丝' | '碎' | '开'
                                | '断' | '除' | '割' | '换' | '线' | '磋' | '花' | '葱'
                                | '姜' | '蒜' | '蛋' | '鱼' | '鸡' | '鸭' | '牛' | '羊'
                                | '猪' | '豆'
                        )
                    ) =>
                {
                    Some("qiē")
                }
                ('好', "hao")
                    if matches!(next, Some('客' | '学' | '胜' | '强' | '战' | '斗' | '逸')) =>
                {
                    Some("hào")
                }
                ('地', "de") => Some("de"),
                ('了', "le") => Some("le"),
                ('着', "zhe") => Some("zhe"),
                ('过', "guo") if i + 1 == n => Some("guo"),
                ('吗', "ma") | ('呢', "ne") | ('吧', "ba") => Some(target.as_str()),
                _ => None,
            };
            if let Some(p) = particle_override {
                resolved.push(p.to_string());
                continue;
            }

            let mut picked: Option<String> = None;
            if let Some(multi) = ch.to_pinyin_multi() {
                for p in multi {
                    let wt = p.with_tone();
                    if normalize_syllable(wt) == *target {
                        picked = Some(wt.to_string());
                        break;
                    }
                }
            } else if let Some(p) = ch.to_pinyin() {
                let wt = p.with_tone();
                if normalize_syllable(wt) == *target {
                    picked = Some(wt.to_string());
                }
            }
            resolved.push(picked.unwrap_or_else(|| target.clone()));
        }

        resolved
    }
}

pub fn default_assets_dir_candidates() -> Option<PathBuf> {
    if let Ok(env_dir) = std::env::var("VITRAL_ASSETS_DIR") {
        let p = PathBuf::from(env_dir.trim());
        if p.is_dir() {
            return Some(p);
        }
    }
    let cwd_assets = PathBuf::from("assets");
    if cwd_assets.join("rime").is_dir() {
        return Some(cwd_assets);
    }
    let manifest_assets = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets");
    if manifest_assets.join("rime").is_dir() {
        return Some(manifest_assets);
    }
    None
}

#[cfg(test)]
mod corpus_tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
        assets: PathBuf,
        dicts: PathBuf,
        cache: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("vitral-corpus-{name}-{}", std::process::id()));
            let assets = root.join("assets");
            let dicts = assets.join("rime/cn_dicts");
            let cache = root.join("data");
            fs::create_dir_all(&dicts).unwrap();
            Self {
                root,
                assets,
                dicts,
                cache,
            }
        }

        fn dict(&self, name: &str, body: &str) {
            fs::write(
                self.dicts.join(name),
                format!("---\nname: test\n...\n{body}"),
            )
            .unwrap();
        }

        fn load(&self) -> Arc<RimeDictCorpus> {
            RimeDictCorpus::load_or_fallback_with_cache(Some(&self.assets), Some(&self.cache))
        }

        fn forget_corpus(&self) {
            let assets = self.assets.canonicalize().unwrap();
            CORPUS_CACHE
                .get()
                .unwrap()
                .lock()
                .retain(|(path, _), _| path.as_ref() != Some(&assets));
        }

        fn cache_file(&self) -> PathBuf {
            let entries: Vec<_> = self
                .cache
                .read_dir()
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].extension().unwrap(), "bin");
            entries.into_iter().next().unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if CORPUS_CACHE.get().is_some() {
                self.forget_corpus();
            }
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn owned_and_mapped_corpora_preserve_readings_weights_and_order() {
        let f = Fixture::new("roundtrip");
        f.dict("8105.dict.yaml", "行\txing\t91\n行\thang\t91\n");
        f.dict("41448.dict.yaml", "行\txing\t999\n");
        let long = "𠀀".repeat(17);
        f.dict("base.dict.yaml", &format!(
            "重庆\tzhòng qìng\t30\n重庆\tchong qing\t40\n重请\tchong qing\t40\n重庆\tchong qing\t15\n崇敬\tchong jing\t50\n沉静\tchen jing\t50\n重庆市\tchong qing shi\t60\n女儿\tnǚ ér\t5\n{long}\t{}\t200\n",
            ["he"; 17].join(" "),
        ));
        let owned = RimeDictCorpus::load_or_fallback(Some(&f.assets));
        assert!(!owned.is_mapped());
        assert!(Arc::ptr_eq(&f.load(), &owned));
        f.forget_corpus();
        let mapped = f.load();
        assert!(mapped.is_mapped());

        for corpus in [owned, mapped] {
            assert_eq!(corpus.max_word_chars(), 12);
            assert_eq!(
                corpus.char_readings('行').collect::<Vec<_>>(),
                [("xing", 91), ("hang", 91)]
            );
            let (syllables, weight) = corpus.word_reading("重庆").unwrap();
            assert_eq!(weight, 40);
            assert_eq!(syllables.collect::<Vec<_>>(), ["chong", "qing"]);
            assert_eq!(
                corpus.word_reading("女儿").unwrap().0.collect::<Vec<_>>(),
                ["nv", "er"]
            );
            assert_eq!(corpus.spaced_pinyin_words("xing").next(), Some(("行", 999)));
            assert_eq!(
                corpus.spaced_pinyin_words("chong qing").collect::<Vec<_>>(),
                [("重庆", 40), ("重请", 40)]
            );
            assert_eq!(
                corpus.spaced_pinyin_words("zhong qing").next(),
                Some(("重庆", 30))
            );
            assert_eq!(
                corpus.initial_readings("cj").collect::<Vec<_>>(),
                ["chen jing", "chong jing"]
            );
            assert_eq!(
                corpus.abbreviations("cq").collect::<Vec<_>>(),
                [("重庆", 40), ("重请", 40)]
            );
            assert_eq!(
                corpus.continuations("重").collect::<Vec<_>>(),
                [("庆市", 60), ("庆", 40), ("请", 40)]
            );
            assert_eq!(corpus.continuations("重庆").next(), Some(("市", 60)));
            assert_eq!(corpus.lookup_word_chars(&['重', '庆']), Some(40));
            assert_eq!(corpus.lookup_word_chars(&['重', '量']), None);
            assert_eq!(corpus.lookup_word_chars(&['𠀀'; 17]), Some(200));
            assert_eq!(corpus.lookup_joined_str("重", "庆"), Some(40));
            assert_eq!(corpus.lookup_joined_str("", ""), None);
            assert_eq!(corpus.lookup_joined_str("𠀀", &"𠀀".repeat(16)), Some(200));
        }
    }

    #[test]
    fn invalid_disk_cache_is_rebuilt() {
        let f = Fixture::new("invalid");
        f.dict("base.dict.yaml", "重庆\tchong qing\t40\n");
        assert!(!f.load().is_mapped());
        let path = f.cache_file();
        let valid = fs::read(&path).unwrap();
        let fingerprint = source_fingerprint(Some(&f.dicts));
        let mut invalid = vec![
            valid[..valid.len() - 1].to_vec(),
            valid.clone(),
            valid.clone(),
            valid.clone(),
        ];
        invalid[1][0] ^= 1; // magic
        invalid[2][16] ^= 1; // source fingerprint
        *invalid[3].last_mut().unwrap() ^= 1; // payload
        invalid.extend(compact::invalid_layouts(&valid));
        for bytes in invalid {
            f.forget_corpus();
            fs::write(&path, bytes).unwrap();
            assert!(RimeDictCorpus::open(&path, &fingerprint).is_none());
            let rebuilt = f.load();
            assert!(!rebuilt.is_mapped());
            assert_eq!(rebuilt.word_weight("重庆"), Some(40));
            assert_eq!(fs::read(&path).unwrap(), valid);
        }
    }

    #[test]
    fn reloading_same_source_path_invalidates_owned_and_mapped_readers() {
        let f = Fixture::new("reload");
        f.dict("base.dict.yaml", "重庆\tchong qing\t40\n");
        f.load();
        f.forget_corpus();
        let mut engine = crate::eng::pure_rust::PureRustEngine::with_corpus_cache(
            Some(&f.assets),
            Some(&f.cache),
        );
        let mut imported =
            crate::imp::ImportedLexiconIndex::with_corpus_cache(Some(&f.assets), Some(&f.cache));
        let old = Arc::clone(engine.get_corpus());
        assert!(old.is_mapped());
        assert!(Arc::ptr_eq(imported.corpus(), &old));

        f.dict("base.dict.yaml", "重庆市\tchong qing shi\t60\n");
        engine.reload_assets(Some(&f.assets));
        imported.reload_assets(Some(&f.assets));
        let updated = engine.get_corpus();
        assert!(Arc::ptr_eq(updated, imported.corpus()));
        assert_eq!(updated.word_weight("重庆"), None);
        assert_eq!(updated.word_weight("重庆市"), Some(60));
        assert_eq!(old.word_weight("重庆"), Some(40));
        assert_eq!(old.word_weight("重庆市"), None);
        let count = CORPUS_CACHE
            .get()
            .unwrap()
            .lock()
            .keys()
            .filter(|(assets, _)| assets.as_ref() == Some(&f.assets))
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn unavailable_disk_cache_uses_owned_corpus() {
        let f = Fixture::new("unavailable");
        f.dict("base.dict.yaml", "重庆\tchong qing\t40\n");
        fs::write(&f.cache, []).unwrap();
        let corpus = f.load();
        assert!(!corpus.is_mapped());
        assert_eq!(corpus.word_weight("重庆"), Some(40));
    }

    #[test]
    fn homophone_bucket_limits_distinguish_single_and_multiple_syllables() {
        let root =
            std::env::temp_dir().join(format!("vitral-homophone-bounds-{}", std::process::id()));
        let dict_dir = root.join("rime/cn_dicts");
        fs::create_dir_all(&dict_dir).unwrap();
        let mut dict = String::from("---\nname: test\n...\n");
        for i in 0..130 {
            let ch = char::from_u32(0x4e00 + i).unwrap();
            for (word, reading) in [(ch.to_string(), "shi"), (format!("是{ch}"), "shi shi")] {
                dict.push_str(&format!("{word}\t{reading}\t{}\n", 1_000 - i));
            }
        }
        fs::write(dict_dir.join("base.dict.yaml"), dict).unwrap();
        let corpus = RimeDictCorpus::load_or_fallback(Some(&root));
        fs::remove_dir_all(root).unwrap();
        assert_eq!(corpus.spaced_pinyin_words("shi").len(), 120);
        assert_eq!(corpus.spaced_pinyin_words("shi shi").len(), 48);
        assert_eq!(corpus.spaced_pinyin_words("shi").nth(119).unwrap().1, 881);
        assert_eq!(
            corpus.spaced_pinyin_words("shi shi").nth(47).unwrap().1,
            953
        );
    }

    #[test]
    fn continuation_buckets_keep_wider_single_character_coverage() {
        let root =
            std::env::temp_dir().join(format!("vitral-continuation-bounds-{}", std::process::id()));
        let dict_dir = root.join("rime/cn_dicts");
        fs::create_dir_all(&dict_dir).unwrap();
        let mut dict = String::from("---\nname: test\n...\n");
        for i in 0..30 {
            let ch = char::from_u32(0x4e00 + i).unwrap();
            for (prefix, reading) in [
                ("正", "zheng"),
                ("人工", "ren gong"),
                ("输入法", "shu ru fa"),
            ] {
                dict.push_str(&format!("{prefix}{ch}\t{reading} que\t{}\n", 100 - i));
            }
        }
        fs::write(dict_dir.join("base.dict.yaml"), dict).unwrap();
        let corpus = RimeDictCorpus::load_or_fallback(Some(&root));
        fs::remove_dir_all(root).unwrap();
        for (prefix, limit) in [("正", 24), ("人工", 10), ("输入法", 10)] {
            let bucket: Vec<_> = corpus.continuations(prefix).collect();
            assert_eq!(bucket.len(), limit);
            assert_eq!(bucket[0].1, 100);
            assert_eq!(bucket[limit - 1].1, 101 - limit as u64);
        }
    }
}
