use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// Strip Unicode tone marks from a pinyin syllable, converting `ü`/`ǖ`/`ǘ`/`ǚ`/`ǜ` -> `v`
/// (and preserving `n`/`l` + `v`, while mapping `[jqxy]v` -> `[jqxy]u` for canonical Rime form).
pub fn normalize_syllable(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.trim().chars() {
        let mapped = match ch {
            'ā' | 'á' | 'ǎ' | 'à' | 'Ā' | 'Á' | 'À' => 'a',
            'ē' | 'é' | 'ě' | 'è' | 'Ē' | 'É' | 'È' => 'e',
            'ī' | 'í' | 'ǐ' | 'ì' | 'Ī' | 'Í' | 'Ì' => 'i',
            'ō' | 'ó' | 'ǒ' | 'ò' | 'Ō' | 'Ó' | 'Ò' => 'o',
            'ū' | 'ú' | 'ǔ' | 'ù' | 'Ū' | 'Ú' | 'Ù' => 'u',
            'ǖ' | 'ǘ' | 'ǚ' | 'ǜ' | 'ü' | 'Ü' => 'v',
            'ń' | 'ň' | 'ǹ' => 'n',
            'ḿ' => 'm',
            c if c.is_ascii_alphabetic() => c.to_ascii_lowercase(),
            c if c.is_ascii_digit() => continue, // strip tone digits 1-5
            '\'' => continue,
            _ => continue,
        };
        out.push(mapped);
    }
    // Canonical Rime spelling: j/q/x/y + v -> u (e.g. ju, jue, juan, jun)
    if let Some(first) = out.chars().next() {
        if matches!(first, 'j' | 'q' | 'x' | 'y') && out.contains('v') {
            out = out.replace('v', "u");
        }
    }
    out
}

/// Convert a single pinyin syllable (toned or untoned) to Xiaohe Shuangpin (`double_pinyin_flypy`) 2-letter code.
pub fn syllable_to_xiaohe(raw: &str) -> String {
    let s = normalize_syllable(raw);
    if s.is_empty() {
        return String::new();
    }
    if s.len() == 1 {
        return match s.as_str() {
            "a" => "aa".to_string(),
            "e" => "ee".to_string(),
            "o" => "oo".to_string(),
            _ => s,
        };
    }

    // Zero-initial rules from double_pinyin_flypy.schema.yaml:
    // xform/^([aoe])(ng)?$/$1$1$2/
    match s.as_str() {
        "ang" => return "ah".to_string(),
        "eng" => return "eg".to_string(),
        "ai" | "ei" | "an" | "en" | "ao" | "ou" | "er" => return s,
        _ => {}
    }

    let (initial_code, rest) = if let Some(r) = s.strip_prefix("zh") {
        ('v', r)
    } else if let Some(r) = s.strip_prefix("ch") {
        ('i', r)
    } else if let Some(r) = s.strip_prefix("sh") {
        ('u', r)
    } else {
        let mut chars = s.chars();
        let first = chars.next().unwrap_or('a');
        (first, chars.as_str())
    };

    let final_code = match rest {
        "" => return initial_code.to_string(),
        "a" => 'a',
        "e" => 'e',
        "i" => 'i',
        "o" => 'o',
        "u" => 'u',
        "v" => 'v',
        "iu" => 'q',
        "ei" => 'w',
        "uan" => 'r',
        "ue" | "ve" => 't',
        "un" => 'y',
        "uo" => 'o',
        "ie" => 'p',
        "ong" | "iong" => 's',
        "ing" | "uai" => 'k',
        "ai" => 'd',
        "en" => 'f',
        "eng" => 'g',
        "iang" | "uang" => 'l',
        "ang" => 'h',
        "ian" => 'm',
        "an" => 'j',
        "ou" => 'z',
        "ia" | "ua" => 'x',
        "iao" => 'n',
        "ao" => 'c',
        "ui" => 'v',
        "in" => 'b',
        _ => {
            // Fallback for non-standard syllables
            return if s.len() <= 2 {
                s
            } else {
                format!("{}{}", initial_code, rest.chars().last().unwrap_or('a'))
            };
        }
    };

    let mut code = String::with_capacity(2);
    code.push(initial_code);
    code.push(final_code);
    code
}

/// Convert space-separated pinyin syllables to concatenated Xiaohe Shuangpin keystrokes.
pub fn pinyin_phrase_to_xiaohe(pinyin_phrase: &str) -> String {
    pinyin_phrase
        .split_whitespace()
        .map(syllable_to_xiaohe)
        .collect::<Vec<_>>()
        .join("")
}

/// Complete canonical Mandarin pinyin syllable inventory (414 syllables using Rime `v` for `nü/lü`).
pub const CANONICAL_SYLLABLES: &[&str] = &[
    "a", "ai", "an", "ang", "ao",
    "ba", "bai", "ban", "bang", "bao", "bei", "ben", "beng", "bi", "bian", "biao", "bie", "bin", "bing", "bo", "bu",
    "ca", "cai", "can", "cang", "cao", "ce", "cen", "ceng", "cha", "chai", "chan", "chang", "chao", "che", "chen", "cheng", "chi", "chong", "chou", "chu", "chua", "chuai", "chuan", "chuang", "chui", "chun", "chuo", "ci", "cong", "cou", "cu", "cuan", "cui", "cun", "cuo",
    "da", "dai", "dan", "dang", "dao", "de", "dei", "den", "deng", "di", "dia", "dian", "diao", "die", "ding", "diu", "dong", "dou", "du", "duan", "dui", "dun", "duo",
    "e", "ei", "en", "eng", "er",
    "fa", "fan", "fang", "fei", "fen", "feng", "fiao", "fo", "fou", "fu",
    "ga", "gai", "gan", "gang", "gao", "ge", "gei", "gen", "geng", "gong", "gou", "gu", "gua", "guai", "guan", "guang", "gui", "gun", "guo",
    "ha", "hai", "han", "hang", "hao", "he", "hei", "hen", "heng", "hm", "hng", "hong", "hou", "hu", "hua", "huai", "huan", "huang", "hui", "hun", "huo",
    "ji", "jia", "jian", "jiang", "jiao", "jie", "jin", "jing", "jiong", "jiu", "ju", "juan", "jue", "jun",
    "ka", "kai", "kan", "kang", "kao", "ke", "kei", "ken", "keng", "kong", "kou", "ku", "kua", "kuai", "kuan", "kuang", "kui", "kun", "kuo",
    "la", "lai", "lan", "lang", "lao", "le", "lei", "leng", "li", "lia", "lian", "liang", "liao", "lie", "lin", "ling", "liu", "lo", "long", "lou", "lu", "luan", "lun", "luo", "lv", "lve",
    "m", "ma", "mai", "man", "mang", "mao", "me", "mei", "men", "meng", "mi", "mian", "miao", "mie", "min", "ming", "miu", "mo", "mou", "mu",
    "n", "na", "nai", "nan", "nang", "nao", "ne", "nei", "nen", "neng", "ng", "ni", "nian", "niang", "niao", "nie", "nin", "ning", "niu", "nong", "nou", "nu", "nuan", "nun", "nuo", "nv", "nve",
    "o", "ou",
    "pa", "pai", "pan", "pang", "pao", "pei", "pen", "peng", "pi", "pian", "piao", "pie", "pin", "ping", "po", "pou", "pu",
    "qi", "qia", "qian", "qiang", "qiao", "qie", "qin", "qing", "qiong", "qiu", "qu", "quan", "que", "qun",
    "ran", "rang", "rao", "re", "ren", "reng", "ri", "rong", "rou", "ru", "rua", "ruan", "rui", "run", "ruo",
    "sa", "sai", "san", "sang", "sao", "se", "sen", "seng", "sha", "shai", "shan", "shang", "shao", "she", "shei", "shen", "sheng", "shi", "shou", "shu", "shua", "shuai", "shuan", "shuang", "shui", "shun", "shuo", "si", "song", "sou", "su", "suan", "sui", "sun", "suo",
    "ta", "tai", "tan", "tang", "tao", "te", "tei", "teng", "ti", "tian", "tiao", "tie", "ting", "tong", "tou", "tu", "tuan", "tui", "tun", "tuo",
    "wa", "wai", "wan", "wang", "wei", "wen", "weng", "wo", "wu",
    "xi", "xia", "xian", "xiang", "xiao", "xie", "xin", "xing", "xiong", "xiu", "xu", "xuan", "xue", "xun",
    "ya", "yan", "yang", "yao", "ye", "yi", "yin", "ying", "yo", "yong", "you", "yu", "yuan", "yue", "yun",
    "za", "zai", "zan", "zang", "zao", "ze", "zei", "zen", "zeng", "zha", "zhai", "zhan", "zhang", "zhao", "zhe", "zhei", "zhen", "zheng", "zhi", "zhong", "zhou", "zhu", "zhua", "zhuai", "zhuan", "zhuang", "zhui", "zhun", "zhuo", "zi", "zong", "zou", "zu", "zuan", "zui", "zun", "zuo",
];

pub fn canonical_syllable_set() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| CANONICAL_SYLLABLES.iter().copied().collect())
}

/// Reverse Xiaohe 2-letter code -> canonical pinyin syllables (including `derive` rules in `double_pinyin_flypy.schema.yaml`).
pub fn xiaohe_reverse_map() -> &'static HashMap<String, Vec<&'static str>> {
    static MAP: OnceLock<HashMap<String, Vec<&'static str>>> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut map: HashMap<String, Vec<&'static str>> = HashMap::new();
        for &syl in CANONICAL_SYLLABLES {
            let code = syllable_to_xiaohe(syl);
            map.entry(code).or_default().push(syl);
        }
        // `derive/^([jqxy])u$/$1v/`
        for (alias, canon) in [("jv", "ju"), ("qv", "qu"), ("xv", "xu"), ("yv", "yu")] {
            map.entry(alias.to_string()).or_default().push(canon);
        }
        // `derive/^([aoe])([ioun])$/$1$1$2/` derived codes
        for (alias, canon) in [
            ("ad", "ai"),
            ("aj", "an"),
            ("ac", "ao"),
            ("ew", "ei"),
            ("ef", "en"),
            ("oz", "ou"),
        ] {
            map.entry(alias.to_string()).or_default().push(canon);
        }
        map
    })
}
