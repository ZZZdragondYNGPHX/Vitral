//! Qwen/GPT-2 byte-level BPE, without a tokenizer runtime dependency.
use crate::error::VitralError;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

type Result<T> = std::result::Result<T, VitralError>;
fn error(e: impl std::fmt::Display) -> VitralError {
    VitralError::Parse(format!("Qwen tokenizer: {e}"))
}

pub struct QwenBpe {
    vocab: HashMap<String, i64>,
    tokens: HashMap<i64, String>,
    ranks: HashMap<(String, String), usize>,
    specials: Vec<String>,
    special_ids: HashSet<i64>,
    bytes: Vec<char>,
    reverse: HashMap<char, u8>,
}
impl QwenBpe {
    pub fn from_tokenizer_json(path: &Path) -> Result<Self> {
        let v: Value =
            serde_json::from_slice(&std::fs::read(path).map_err(error)?).map_err(error)?;
        Self::load(
            &v["model"]["vocab"],
            &v["model"]["merges"],
            &v["added_tokens"],
        )
    }
    pub fn from_dir(dir: &Path) -> Result<Self> {
        let vocab: Value =
            serde_json::from_slice(&std::fs::read(dir.join("vocab.json")).map_err(error)?)
                .map_err(error)?;
        let merges = std::fs::read_to_string(dir.join("merges.txt")).map_err(error)?;
        let merges = Value::Array(
            merges
                .lines()
                .filter(|s| !s.starts_with('#') && !s.is_empty())
                .map(|s| Value::String(s.into()))
                .collect(),
        );
        let cfg: Value = serde_json::from_slice(
            &std::fs::read(dir.join("tokenizer_config.json")).map_err(error)?,
        )
        .map_err(error)?;
        let added = Value::Array(
            cfg["added_tokens_decoder"]
                .as_object()
                .ok_or_else(|| error("missing added_tokens_decoder"))?
                .iter()
                .map(|(id, t)| {
                    let mut t = t.clone();
                    t["id"] = id.parse::<i64>().map(Value::from).unwrap_or(Value::Null);
                    t
                })
                .collect(),
        );
        Self::load(&vocab, &merges, &added)
    }
    fn load(vocab: &Value, merges: &Value, added: &Value) -> Result<Self> {
        let mut vocab: HashMap<String, i64> =
            serde_json::from_value(vocab.clone()).map_err(error)?;
        let mut specials = Vec::new();
        let mut special_ids = HashSet::new();
        if let Some(added) = added.as_array() {
            for t in added {
                let token = t["content"]
                    .as_str()
                    .ok_or_else(|| error("invalid added token"))?;
                let id = t["id"]
                    .as_i64()
                    .ok_or_else(|| error("invalid added token id"))?;
                vocab.insert(token.into(), id);
                // Added tokens must be isolated even when HF doesn't mark them special.
                specials.push(token.to_owned());
                if t["special"].as_bool().unwrap_or(false) {
                    special_ids.insert(id);
                }
            }
        }
        specials.sort_by_key(|s| std::cmp::Reverse(s.len()));
        let mut ranks = HashMap::new();
        for (rank, m) in merges
            .as_array()
            .ok_or_else(|| error("missing merges"))?
            .iter()
            .enumerate()
        {
            let pair = if let Some(s) = m.as_str() {
                s.split_once(' ').map(|(a, b)| (a.to_owned(), b.to_owned()))
            } else {
                m.as_array()
                    .and_then(|a| Some((a.first()?.as_str()?.into(), a.get(1)?.as_str()?.into())))
            };
            let pair = pair.ok_or_else(|| error("invalid merge"))?;
            ranks.insert(pair, rank);
        }
        let mut bytes = vec!['\0'; 256];
        let mut extra = 256u32;
        for b in 0..256u32 {
            let code = if (33..=126).contains(&b)
                || (161..=172).contains(&b)
                || (174..=255).contains(&b)
            {
                b
            } else {
                let n = extra;
                extra += 1;
                n
            };
            bytes[b as usize] = char::from_u32(code).unwrap();
        }
        let reverse = bytes
            .iter()
            .enumerate()
            .map(|(b, &c)| (c, b as u8))
            .collect();
        let tokens = vocab.iter().map(|(s, &id)| (id, s.clone())).collect();
        Ok(Self {
            vocab,
            tokens,
            ranks,
            specials,
            special_ids,
            bytes,
            reverse,
        })
    }
    pub fn token_id(&self, token: &str) -> Option<i64> {
        self.vocab.get(token).copied()
    }
    pub fn encode(&self, text: &str) -> Result<Vec<i64>> {
        let mut out = Vec::new();
        let mut rest = text;
        while !rest.is_empty() {
            let special = self
                .specials
                .iter()
                .filter_map(|s| rest.find(s).map(|p| (p, s)))
                .min_by_key(|(p, s)| (*p, std::cmp::Reverse(s.len())));
            let end = special.as_ref().map_or(rest.len(), |(p, _)| *p);
            for piece in pre_tokens(&rest[..end]) {
                let mut parts: Vec<String> = piece
                    .bytes()
                    .map(|b| self.bytes[b as usize].to_string())
                    .collect();
                loop {
                    let best = parts
                        .windows(2)
                        .enumerate()
                        .filter_map(|(i, p)| {
                            self.ranks
                                .get(&(p[0].clone(), p[1].clone()))
                                .map(|&r| (r, i))
                        })
                        .min();
                    let Some((_, i)) = best else { break };
                    let right = parts.remove(i + 1);
                    parts[i].push_str(&right);
                }
                for p in parts {
                    out.push(
                        *self
                            .vocab
                            .get(&p)
                            .ok_or_else(|| error(format!("unknown byte piece {p}")))?,
                    );
                }
            }
            if let Some((at, s)) = special {
                out.push(self.vocab[s]);
                rest = &rest[at + s.len()..];
            } else {
                break;
            }
        }
        Ok(out)
    }
    pub fn decode(&self, ids: &[i64], skip_special: bool) -> String {
        let mut bytes = Vec::new();
        for id in ids {
            self.append_token_bytes(*id, skip_special, &mut bytes);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    pub(super) fn append_token_bytes(&self, id: i64, skip_special: bool, bytes: &mut Vec<u8>) {
        if skip_special && self.special_ids.contains(&id) {
            return;
        }
        if let Some(token) = self.tokens.get(&id) {
            if self.specials.contains(token) {
                bytes.extend_from_slice(token.as_bytes());
            } else {
                bytes.extend(token.chars().filter_map(|ch| self.reverse.get(&ch)));
            }
        }
    }
}
// Equivalent to Qwen's pre-tokenization pattern. Keep Unicode letters together,
// isolate digits, preserve punctuation/newline groups and optional leading space.
// Unicode Alphabetic includes combining marks and letter numbers that Qwen's
// \\p{L} pre-tokenizer excludes. Half-open range boundaries keep the lookup small.
const OTHER_ALPHABETIC: &[u32] = &[
    0x345, 0x346, 0x363, 0x370, 0x5b0, 0x5be, 0x5bf, 0x5c0, 0x5c1, 0x5c3, 0x5c4, 0x5c6, 0x5c7,
    0x5c8, 0x610, 0x61b, 0x64b, 0x658, 0x659, 0x660, 0x670, 0x671, 0x6d6, 0x6dd, 0x6e1, 0x6e5,
    0x6e7, 0x6e9, 0x6ed, 0x6ee, 0x711, 0x712, 0x730, 0x740, 0x7a6, 0x7b1, 0x816, 0x818, 0x81b,
    0x824, 0x825, 0x828, 0x829, 0x82d, 0x897, 0x898, 0x8d4, 0x8e0, 0x8e3, 0x8ea, 0x8f0, 0x904,
    0x93a, 0x93c, 0x93e, 0x94d, 0x94e, 0x950, 0x955, 0x958, 0x962, 0x964, 0x981, 0x984, 0x9be,
    0x9c5, 0x9c7, 0x9c9, 0x9cb, 0x9cd, 0x9d7, 0x9d8, 0x9e2, 0x9e4, 0xa01, 0xa04, 0xa3e, 0xa43,
    0xa47, 0xa49, 0xa4b, 0xa4d, 0xa51, 0xa52, 0xa70, 0xa72, 0xa75, 0xa76, 0xa81, 0xa84, 0xabe,
    0xac6, 0xac7, 0xaca, 0xacb, 0xacd, 0xae2, 0xae4, 0xafa, 0xafd, 0xb01, 0xb04, 0xb3e, 0xb45,
    0xb47, 0xb49, 0xb4b, 0xb4d, 0xb56, 0xb58, 0xb62, 0xb64, 0xb82, 0xb83, 0xbbe, 0xbc3, 0xbc6,
    0xbc9, 0xbca, 0xbcd, 0xbd7, 0xbd8, 0xc00, 0xc05, 0xc3e, 0xc45, 0xc46, 0xc49, 0xc4a, 0xc4d,
    0xc55, 0xc57, 0xc62, 0xc64, 0xc81, 0xc84, 0xcbe, 0xcc5, 0xcc6, 0xcc9, 0xcca, 0xccd, 0xcd5,
    0xcd7, 0xce2, 0xce4, 0xcf3, 0xcf4, 0xd00, 0xd04, 0xd3e, 0xd45, 0xd46, 0xd49, 0xd4a, 0xd4d,
    0xd57, 0xd58, 0xd62, 0xd64, 0xd81, 0xd84, 0xdcf, 0xdd5, 0xdd6, 0xdd7, 0xdd8, 0xde0, 0xdf2,
    0xdf4, 0xe31, 0xe32, 0xe34, 0xe3b, 0xe4d, 0xe4e, 0xeb1, 0xeb2, 0xeb4, 0xeba, 0xebb, 0xebd,
    0xecd, 0xece, 0xf71, 0xf84, 0xf8d, 0xf98, 0xf99, 0xfbd, 0x102b, 0x1037, 0x1038, 0x1039, 0x103b,
    0x103f, 0x1056, 0x105a, 0x105e, 0x1061, 0x1062, 0x1065, 0x1067, 0x106e, 0x1071, 0x1075, 0x1082,
    0x108e, 0x108f, 0x1090, 0x109a, 0x109e, 0x16ee, 0x16f1, 0x1712, 0x1714, 0x1732, 0x1734, 0x1752,
    0x1754, 0x1772, 0x1774, 0x17b6, 0x17c9, 0x1885, 0x1887, 0x18a9, 0x18aa, 0x1920, 0x192c, 0x1930,
    0x1939, 0x1a17, 0x1a1c, 0x1a55, 0x1a5f, 0x1a61, 0x1a75, 0x1abf, 0x1ac1, 0x1acc, 0x1acf, 0x1b00,
    0x1b05, 0x1b35, 0x1b44, 0x1b80, 0x1b83, 0x1ba1, 0x1baa, 0x1bac, 0x1bae, 0x1be7, 0x1bf2, 0x1c24,
    0x1c37, 0x1dd3, 0x1df5, 0x2160, 0x2183, 0x2185, 0x2189, 0x24b6, 0x24ea, 0x2de0, 0x2e00, 0x3007,
    0x3008, 0x3021, 0x302a, 0x3038, 0x303b, 0xa674, 0xa67c, 0xa69e, 0xa6a0, 0xa6e6, 0xa6f0, 0xa802,
    0xa803, 0xa80b, 0xa80c, 0xa823, 0xa828, 0xa880, 0xa882, 0xa8b4, 0xa8c4, 0xa8c5, 0xa8c6, 0xa8ff,
    0xa900, 0xa926, 0xa92b, 0xa947, 0xa953, 0xa980, 0xa984, 0xa9b4, 0xa9c0, 0xa9e5, 0xa9e6, 0xaa29,
    0xaa37, 0xaa43, 0xaa44, 0xaa4c, 0xaa4e, 0xaa7b, 0xaa7e, 0xaab0, 0xaab1, 0xaab2, 0xaab5, 0xaab7,
    0xaab9, 0xaabe, 0xaabf, 0xaaeb, 0xaaf0, 0xaaf5, 0xaaf6, 0xabe3, 0xabeb, 0xfb1e, 0xfb1f,
    0x10140, 0x10175, 0x10341, 0x10342, 0x1034a, 0x1034b, 0x10376, 0x1037b, 0x103d1, 0x103d6,
    0x10a01, 0x10a04, 0x10a05, 0x10a07, 0x10a0c, 0x10a10, 0x10d24, 0x10d28, 0x10d69, 0x10d6a,
    0x10eab, 0x10ead, 0x10efa, 0x10efd, 0x11000, 0x11003, 0x11038, 0x11046, 0x11073, 0x11075,
    0x11080, 0x11083, 0x110b0, 0x110b9, 0x110c2, 0x110c3, 0x11100, 0x11103, 0x11127, 0x11133,
    0x11145, 0x11147, 0x11180, 0x11183, 0x111b3, 0x111c0, 0x111ce, 0x111d0, 0x1122c, 0x11235,
    0x11237, 0x11238, 0x1123e, 0x1123f, 0x11241, 0x11242, 0x112df, 0x112e9, 0x11300, 0x11304,
    0x1133e, 0x11345, 0x11347, 0x11349, 0x1134b, 0x1134d, 0x11357, 0x11358, 0x11362, 0x11364,
    0x113b8, 0x113c1, 0x113c2, 0x113c3, 0x113c5, 0x113c6, 0x113c7, 0x113cb, 0x113cc, 0x113ce,
    0x11435, 0x11442, 0x11443, 0x11446, 0x114b0, 0x114c2, 0x115af, 0x115b6, 0x115b8, 0x115bf,
    0x115dc, 0x115de, 0x11630, 0x1163f, 0x11640, 0x11641, 0x116ab, 0x116b6, 0x1171d, 0x1172b,
    0x1182c, 0x11839, 0x11930, 0x11936, 0x11937, 0x11939, 0x1193b, 0x1193d, 0x11940, 0x11941,
    0x11942, 0x11943, 0x119d1, 0x119d8, 0x119da, 0x119e0, 0x119e4, 0x119e5, 0x11a01, 0x11a0b,
    0x11a35, 0x11a3a, 0x11a3b, 0x11a3f, 0x11a51, 0x11a5c, 0x11a8a, 0x11a98, 0x11b60, 0x11b68,
    0x11c2f, 0x11c37, 0x11c38, 0x11c3f, 0x11c92, 0x11ca8, 0x11ca9, 0x11cb7, 0x11d31, 0x11d37,
    0x11d3a, 0x11d3b, 0x11d3c, 0x11d3e, 0x11d3f, 0x11d42, 0x11d43, 0x11d44, 0x11d47, 0x11d48,
    0x11d8a, 0x11d8f, 0x11d90, 0x11d92, 0x11d93, 0x11d97, 0x11ef3, 0x11ef7, 0x11f00, 0x11f02,
    0x11f03, 0x11f04, 0x11f34, 0x11f3b, 0x11f3e, 0x11f41, 0x12400, 0x1246f, 0x1611e, 0x1612f,
    0x16f4f, 0x16f50, 0x16f51, 0x16f88, 0x16f8f, 0x16f93, 0x16ff0, 0x16ff2, 0x16ff4, 0x16ff7,
    0x1bc9e, 0x1bc9f, 0x1e000, 0x1e007, 0x1e008, 0x1e019, 0x1e01b, 0x1e022, 0x1e023, 0x1e025,
    0x1e026, 0x1e02b, 0x1e08f, 0x1e090, 0x1e6e3, 0x1e6e4, 0x1e6e6, 0x1e6e7, 0x1e6ee, 0x1e6f0,
    0x1e6f5, 0x1e6f6, 0x1e947, 0x1e948, 0x1f130, 0x1f14a, 0x1f150, 0x1f16a, 0x1f170, 0x1f18a,
];
fn is_letter(c: char) -> bool {
    c.is_alphabetic() && OTHER_ALPHABETIC.partition_point(|&edge| edge <= c as u32) % 2 == 0
}
fn pre_tokens(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < text.len() {
        let rest = &text[at..];
        if let Some(s) = ["'s", "'t", "'re", "'ve", "'m", "'ll", "'d"]
            .into_iter()
            .find(|s| {
                rest.get(..s.len())
                    .is_some_and(|r| r.eq_ignore_ascii_case(s))
            })
        {
            out.push(&text[at..at + s.len()]);
            at += s.len();
            continue;
        }
        let chars: Vec<(usize, char)> = rest.char_indices().collect();
        let c = chars[0].1;
        let letter = is_letter;
        let mut n = 1;
        if letter(c)
            || (!matches!(c, '\r' | '\n')
                && !(letter(c) || c.is_numeric())
                && chars.get(1).is_some_and(|(_, c)| letter(*c)))
        {
            while n < chars.len() && letter(chars[n].1) {
                n += 1;
            }
        } else if c.is_numeric() {
        } else if !c.is_whitespace()
            || (c == ' '
                && chars
                    .get(1)
                    .is_some_and(|(_, c)| !c.is_whitespace() && !(letter(*c) || c.is_numeric())))
        {
            while n < chars.len()
                && !chars[n].1.is_whitespace()
                && !(letter(chars[n].1) || chars[n].1.is_numeric())
            {
                n += 1;
            }
            while n < chars.len() && matches!(chars[n].1, '\r' | '\n') {
                n += 1;
            }
        } else {
            while n < chars.len() && chars[n].1.is_whitespace() {
                n += 1;
            }
            if let Some(last) = chars[..n]
                .iter()
                .rposition(|(_, c)| matches!(c, '\r' | '\n'))
            {
                n = last + 1;
            } else if n > 1 && n < chars.len() {
                n -= 1;
            }
        }
        let end = chars.get(n).map_or(rest.len(), |(i, _)| *i);
        out.push(&rest[..end]);
        at += end;
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> QwenBpe {
        let bytes: Vec<char> = (33..=126)
            .chain(161..=172)
            .chain(174..=255)
            .map(|b| char::from_u32(b).unwrap())
            .collect();
        let mut vocab = serde_json::Map::new();
        let mut extra = 256;
        for b in 0..256 {
            let c = char::from_u32(b).unwrap();
            let c = if bytes.contains(&c) {
                c
            } else {
                let c = char::from_u32(extra).unwrap();
                extra += 1;
                c
            };
            vocab.insert(c.to_string(), Value::from(b));
        }
        QwenBpe::load(&Value::Object(vocab),&serde_json::json!([]),&serde_json::json!([
            {"content":"<|im_start|>","id":300,"special":true},{"content":"<|im_end|>","id":301,"special":true},{"content":"<asr_text>","id":302,"special":true}
        ])).unwrap()
    }
    #[test]
    fn unicode_and_special_roundtrip() {
        let t = fixture();
        for s in [
            "你好，世界！ Rust 123\n",
            "<|im_start|>user\n你好<|im_end|>",
            "language Chinese<asr_text>你好🙂",
            "I'm ready.\r\n  next",
        ] {
            assert_eq!(t.decode(&t.encode(s).unwrap(), false), s);
        }
        assert_eq!(t.decode(&[300, 65, 301], true), "A");
        assert_eq!(t.decode(&[0xe4], false), "�");
    }
    #[test]
    fn both_huggingface_layouts_apply_merge_ranks() {
        let mut t = fixture();
        t.vocab.insert("Ġw".into(), 400);
        let added = serde_json::json!([{"id":300,"content":"<|im_start|>","special":true},{"id":301,"content":"<|im_end|>","special":true},{"id":302,"content":"<asr_text>","special":true}]);
        let dir = std::env::temp_dir().join(format!("vitral-bpe-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        struct Clean(std::path::PathBuf);
        impl Drop for Clean {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _clean = Clean(dir.clone());
        std::fs::write(dir.join("tokenizer.json"),serde_json::json!({"model":{"vocab":t.vocab,"merges":[["Ġ","w"]]},"added_tokens":added}).to_string()).unwrap();
        std::fs::write(
            dir.join("vocab.json"),
            serde_json::to_vec(&t.vocab).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.join("merges.txt"), "#version: 0.2\nĠ w\n").unwrap();
        let mut decoder = serde_json::Map::new();
        for token in added.as_array().unwrap() {
            decoder.insert(token["id"].to_string(), token.clone());
        }
        std::fs::write(
            dir.join("tokenizer_config.json"),
            serde_json::json!({"added_tokens_decoder":decoder}).to_string(),
        )
        .unwrap();
        let a = QwenBpe::from_tokenizer_json(&dir.join("tokenizer.json")).unwrap();
        let b = QwenBpe::from_dir(&dir).unwrap();
        let text = "你好 world!<asr_text>";
        let ids = a.encode(text).unwrap();
        assert!(ids.contains(&400));
        assert_eq!(ids, b.encode(text).unwrap());
        assert_eq!(a.decode(&ids, false), text);
    }
    #[test]
    fn pretoken_boundaries() {
        assert_eq!(
            pre_tokens("किताब Ⅳ e\u{301}clair"),
            vec!["क", "ित", "ाब", " ", "Ⅳ", " e", "\u{301}clair"]
        );
        assert_eq!(
            pre_tokens("Hello world 123!\nI'm here"),
            vec!["Hello", " world", " ", "1", "2", "3", "!\n", "I", "'m", " here"]
        );
    }
}
