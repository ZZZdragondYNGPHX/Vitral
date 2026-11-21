use crate::imp::xiaohe::{normalize_syllable, syllable_to_xiaohe};
use parking_lot::Mutex;
use pinyin::{ToPinyin, ToPinyinMulti};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

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

#[derive(Debug)]
pub struct RimeDictCorpus {
    pub char_readings: HashMap<char, Vec<(String, u64)>>,
    pub word_readings: HashMap<String, (Vec<String>, u64)>,
    pub spaced_pinyin_to_words: HashMap<String, Vec<(String, u64)>>,
    pub max_word_chars: usize,
    word_bloom: Vec<u64>,
}

static CORPUS_CACHE: OnceLock<Mutex<HashMap<String, Arc<RimeDictCorpus>>>> = OnceLock::new();

impl RimeDictCorpus {
    pub fn load_or_fallback(assets_dir: Option<&Path>) -> Arc<Self> {
        let key = assets_dir
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "__fallback__".to_string());

        let cache = CORPUS_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        let mut guard = cache.lock();
        if let Some(c) = guard.get(&key) {
            return Arc::clone(c);
        }
        let built = Arc::new(Self::build(assets_dir));
        guard.insert(key, Arc::clone(&built));
        built
    }

    fn build(assets_dir: Option<&Path>) -> Self {
        let mut char_readings: HashMap<char, Vec<(String, u64)>> = HashMap::new();
        let mut word_readings: HashMap<String, (Vec<String>, u64)> = HashMap::new();
        let mut spaced_pinyin_to_words: HashMap<String, Vec<(String, u64)>> = HashMap::new();

        let cn_dicts = assets_dir.map(|d| d.join("rime").join("cn_dicts"));
        let has_dicts = cn_dicts.as_ref().is_some_and(|p| p.is_dir());

        if let Some(dir) = cn_dicts.filter(|_| has_dicts) {
            Self::parse_rime_dict_file(
                &dir.join("8105.dict.yaml"),
                50,
                true,
                &mut char_readings,
                &mut word_readings,
                &mut spaced_pinyin_to_words,
            );
            Self::parse_rime_dict_file(
                &dir.join("41448.dict.yaml"),
                10,
                false,
                &mut char_readings,
                &mut word_readings,
                &mut spaced_pinyin_to_words,
            );
            Self::parse_rime_dict_file(
                &dir.join("base.dict.yaml"),
                100,
                true,
                &mut char_readings,
                &mut word_readings,
                &mut spaced_pinyin_to_words,
            );
            Self::parse_rime_dict_file(
                &dir.join("ext.dict.yaml"),
                80,
                true,
                &mut char_readings,
                &mut word_readings,
                &mut spaced_pinyin_to_words,
            );
            Self::parse_rime_dict_file(
                &dir.join("others.dict.yaml"),
                25,
                false,
                &mut char_readings,
                &mut word_readings,
                &mut spaced_pinyin_to_words,
            );
        }

        // Seed minimal core fallback entries if assets/rime/cn_dicts is absent (e.g., sandboxed unit tests)
        if word_readings.is_empty() {
            Self::seed_core_fallback(
                &mut char_readings,
                &mut word_readings,
                &mut spaced_pinyin_to_words,
            );
        }

        for list in char_readings.values_mut() {
            list.sort_by_key(|a| std::cmp::Reverse(a.1));
        }

        let mut max_word_chars = 1usize;
        for w in word_readings.keys() {
            max_word_chars = max_word_chars.max(w.chars().count());
        }

        // Sort and cap candidate lists per spaced_pinyin key for fast O(1) lattice lookup
        for list in spaced_pinyin_to_words.values_mut() {
            list.sort_by_key(|a| std::cmp::Reverse(a.1));
            let mut seen = HashSet::with_capacity(list.len().min(48));
            list.retain(|(w, _)| seen.insert(w.clone()));
            if list.len() > 48 {
                list.truncate(48);
            }
        }

        char_readings.shrink_to_fit();
        word_readings.shrink_to_fit();
        spaced_pinyin_to_words.shrink_to_fit();

        let mut word_bloom = vec![0u64; CORPUS_BLOOM_WORDS];
        let mut tmp_chars = ['\0'; 5];
        for w in word_readings.keys() {
            let mut cnt = 0usize;
            let mut fit = true;
            for ch in w.chars() {
                if cnt >= 5 {
                    fit = false;
                    break;
                }
                tmp_chars[cnt] = ch;
                cnt += 1;
            }
            if fit && cnt >= 1 {
                if let Some(k) = pack_word_u128(&tmp_chars[..cnt]) {
                    let (h1, h2) = corpus_bloom_hashes(k);
                    for probe in 0..4u64 {
                        let bit = h1.wrapping_add(probe.wrapping_mul(h2)) & (CORPUS_BLOOM_BITS - 1);
                        word_bloom[(bit >> 6) as usize] |= 1u64 << (bit & 63);
                    }
                }
            }
        }

        Self {
            char_readings,
            word_readings,
            spaced_pinyin_to_words,
            max_word_chars: max_word_chars.min(12),
            word_bloom,
        }
    }

    #[inline]
    pub fn may_contain_chars(&self, chars: &[char]) -> bool {
        if chars.is_empty() {
            return false;
        }
        let Some(k) = pack_word_u128(chars) else {
            return true;
        };
        let (h1, h2) = corpus_bloom_hashes(k);
        for probe in 0..4u64 {
            let bit = h1.wrapping_add(probe.wrapping_mul(h2)) & (CORPUS_BLOOM_BITS - 1);
            if (self.word_bloom[(bit >> 6) as usize] & (1u64 << (bit & 63))) == 0 {
                return false;
            }
        }
        true
    }

    #[inline]
    pub fn lookup_word_chars(&self, chars: &[char]) -> Option<u64> {
        if !self.may_contain_chars(chars) {
            return None;
        }
        if chars.len() <= 8 {
            let mut buf = [0u8; 32];
            let mut pos = 0usize;
            for &c in chars {
                let s = c.encode_utf8(&mut buf[pos..]);
                pos += s.len();
            }
            // SAFETY: `encode_utf8` writes valid UTF-8 into `buf[..pos]`.
            let s = unsafe { std::str::from_utf8_unchecked(&buf[..pos]) };
            self.word_readings.get(s).map(|&(_, w)| w)
        } else {
            let s: String = chars.iter().collect();
            self.word_readings.get(&s).map(|&(_, w)| w)
        }
    }

    #[inline]
    pub fn lookup_joined_str(&self, w1: &str, w2: &str) -> Option<u64> {
        let total_bytes = w1.len() + w2.len();
        if total_bytes == 0 {
            return None;
        }
        if total_bytes <= 64 {
            let mut buf = [0u8; 64];
            buf[..w1.len()].copy_from_slice(w1.as_bytes());
            buf[w1.len()..total_bytes].copy_from_slice(w2.as_bytes());
            // SAFETY: `w1` and `w2` are valid UTF-8 slices concatenated sequentially.
            let s = unsafe { std::str::from_utf8_unchecked(&buf[..total_bytes]) };
            let mut chs = ['\0'; 6];
            let mut n = 0usize;
            for ch in s.chars() {
                if n >= 6 {
                    break;
                }
                chs[n] = ch;
                n += 1;
            }
            if n <= 5 && !self.may_contain_chars(&chs[..n]) {
                return None;
            }
            self.word_readings.get(s).map(|&(_, w)| w)
        } else {
            let mut s = String::with_capacity(total_bytes);
            s.push_str(w1);
            s.push_str(w2);
            self.word_readings.get(&s).map(|&(_, w)| w)
        }
    }

    fn parse_rime_dict_file(
        path: &Path,
        default_weight: u64,
        allow_override: bool,
        char_readings: &mut HashMap<char, Vec<(String, u64)>>,
        word_readings: &mut HashMap<String, (Vec<String>, u64)>,
        spaced_pinyin_to_words: &mut HashMap<String, Vec<(String, u64)>>,
    ) {
        let Ok(content) = fs::read_to_string(path) else {
            return;
        };
        let mut in_body = false;

        for line in content.lines() {
            let trimmed = line.trim();
            if !in_body {
                if trimmed == "..." {
                    in_body = true;
                }
                continue;
            }
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let mut cols = line.split('\t');
            let Some(text_raw) = cols.next() else {
                continue;
            };
            let text = text_raw.trim();
            if text.is_empty() {
                continue;
            }
            let Some(code_raw) = cols.next() else {
                continue;
            };
            let syllables: Vec<String> = code_raw
                .split_whitespace()
                .map(normalize_syllable)
                .filter(|s| !s.is_empty())
                .collect();
            if syllables.is_empty() {
                continue;
            }
            let weight = cols
                .next()
                .and_then(|w| w.trim().parse::<u64>().ok())
                .unwrap_or(default_weight)
                .max(1);

            let char_cnt = text.chars().count();
            if char_cnt == 1 && syllables.len() == 1 {
                let ch = text.chars().next().unwrap();
                let list = char_readings.entry(ch).or_default();
                if let Some(existing) = list.iter_mut().find(|(s, _)| s == &syllables[0]) {
                    if allow_override && weight > existing.1 {
                        existing.1 = weight;
                    }
                } else {
                    list.push((syllables[0].clone(), weight));
                }
            }

            if syllables.len() == char_cnt {
                let spaced = syllables.join(" ");
                spaced_pinyin_to_words
                    .entry(spaced)
                    .or_default()
                    .push((text.to_string(), weight));

                match word_readings.get_mut(text) {
                    Some((prev_syls, prev_w)) => {
                        if weight > *prev_w {
                            *prev_syls = syllables;
                            *prev_w = weight;
                        }
                    }
                    None => {
                        word_readings.insert(text.to_string(), (syllables, weight));
                    }
                }
            }
        }
    }

    fn seed_core_fallback(
        char_readings: &mut HashMap<char, Vec<(String, u64)>>,
        word_readings: &mut HashMap<String, (Vec<String>, u64)>,
        spaced_pinyin_to_words: &mut HashMap<String, Vec<(String, u64)>>,
    ) {
        const FALLBACK_CHARS: &[(&str, &str, u64)] = &[
            ("你", "ni", 10_000_000),
            ("好", "hao", 10_000_000),
            ("世", "shi", 5_000_000),
            ("界", "jie", 5_000_000),
            ("我", "wo", 20_000_000),
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
            char_readings
                .entry(ch)
                .or_default()
                .push((syl.to_string(), w));
            spaced_pinyin_to_words
                .entry(syl.to_string())
                .or_default()
                .push((ch_s.to_string(), w));
        }
        const FALLBACK_WORDS: &[(&str, &str, u64)] = &[
            ("你好", "ni hao", 1_000_000),
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
            let syls: Vec<String> = py.split_whitespace().map(str::to_string).collect();
            word_readings.insert(w.to_string(), (syls, freq));
            spaced_pinyin_to_words
                .entry(py.to_string())
                .or_default()
                .push((w.to_string(), freq));
        }
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

        if let Some(dict_list) = self.corpus.char_readings.get(&ch) {
            for (untoned, _) in dict_list {
                // Find matching toned variant from `pinyin` crate
                let mut matched = false;
                for toned in &toned_candidates {
                    if normalize_syllable(toned) == *untoned && seen_toned.insert(toned.clone()) {
                        let xh = syllable_to_xiaohe(toned);
                        results.push((toned.clone(), xh));
                        matched = true;
                    }
                }
                if !matched && toned_candidates.is_empty() && seen_toned.insert(untoned.clone()) {
                    let xh = syllable_to_xiaohe(untoned);
                    results.push((untoned.clone(), xh));
                }
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

        let max_len = self.corpus.max_word_chars.min(n).max(4.min(n));
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
                            .word_readings
                            .get(&span)
                            .map(|(_, w)| *w)
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
                if let Some((syls, weight)) = self.corpus.word_readings.get(&span) {
                    let edge_score = ((*weight as f64) + 1.0).ln()
                        + (len as f64) * 14.0
                        + ((len - 1) as f64) * 4.0;
                    let cand = dp_score[i] + edge_score;
                    if cand > dp_score[i + len] {
                        dp_score[i + len] = cand;
                        dp_prev[i + len] = Some((i, syls.clone()));
                    }
                }
            }

            // Single-char fallback (with contextual neighbor rules for OOV boundaries)
            let ch = chars[i];
            let syl = self.single_char_untoned_with_context(&chars, i);
            let char_w = self
                .corpus
                .char_readings
                .get(&ch)
                .and_then(|v| v.first().map(|x| x.1))
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
            .char_readings
            .get(&ch)
            .and_then(|v| v.first())
            .map(|x| x.0.clone())
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
