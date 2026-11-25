use super::SchemaKind;
use crate::ret::RetentionEngine;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

const CORE_ENGLISH: &[&str] = &[
    "OK",
    "App",
    "iPhone",
    "iPad",
    "Mac",
    "GitHub",
    "VSCode",
    "Python",
    "Rust",
    "Java",
    "Linux",
    "Android",
    "Windows",
    "Wi-Fi",
    "PDF",
    "API",
    "URL",
    "HTTP",
    "JSON",
    "SQL",
    "AI",
    "LLM",
    "CPU",
    "GPU",
    "RAM",
    "USB",
    "VPN",
    "UI",
    "UX",
    "Git",
    "Docker",
    "Chrome",
    "WeChat",
    "Google",
    "Apple",
    "Email",
    "Gmail",
    "Office",
    "Excel",
    "Word",
    "PPT",
    "Hello",
    "Thanks",
    "Sorry",
    "Please",
    "Test",
    "Demo",
    "Beta",
    "Pro",
    "Max",
    "Mini",
    "Plus",
    "VIP",
    "TODO",
    "BUG",
    "Fix",
    "WebDAV",
    "Koofr",
    "Vitral",
    "OpenAI",
    "ChatGPT",
    "HTML",
    "CSS",
    "JavaScript",
    "TypeScript",
    "React",
    "Node",
    "NPM",
    "SDK",
    "IDE",
    "HTTPS",
    "TCP",
    "IP",
    "DNS",
    "SSH",
    "FTP",
    "SSD",
    "HDD",
    "WiFi",
    "Bluetooth",
    "Zoom",
    "CLI",
];

type EnglishCache = Mutex<HashMap<Option<PathBuf>, Arc<EnglishDictionary>>>;
static CACHE: OnceLock<EnglishCache> = OnceLock::new();

#[derive(Debug)]
struct EnglishEntry {
    code: String,
    text: String,
    common: bool,
}

#[derive(Debug)]
pub(super) struct EnglishDictionary {
    words: Vec<EnglishEntry>,
    quanpin: Vec<(String, String)>,
    flypy: Vec<(String, String)>,
}

impl EnglishDictionary {
    pub(super) fn open_cached(assets_dir: Option<&Path>) -> Arc<Self> {
        let key = assets_dir.map(Path::to_owned);
        let mut cache = CACHE.get_or_init(EnglishCache::default).lock();
        Arc::clone(
            cache
                .entry(key)
                .or_insert_with(|| Arc::new(Self::load(assets_dir))),
        )
    }

    pub(super) fn invalidate(assets_dir: Option<&Path>) {
        if let Some(cache) = CACHE.get() {
            cache.lock().remove(&assets_dir.map(Path::to_owned));
        }
    }

    fn load(assets_dir: Option<&Path>) -> Self {
        let mut words: HashMap<_, _> = CORE_ENGLISH
            .iter()
            .map(|&text| (text.to_ascii_lowercase(), (text.to_owned(), true)))
            .collect();
        let mut quanpin = Vec::new();
        let mut flypy = Vec::new();
        if let Some(assets_dir) = assets_dir {
            let dir = assets_dir.join("rime").join("en_dicts");
            for (name, common) in [("en.dict.yaml", true), ("en_ext.dict.yaml", false)] {
                read_rows(&dir.join(name), true, |text, code| {
                    if printable_ascii(code) {
                        // Core spellings take precedence, then the main dictionary.
                        words
                            .entry(code.to_ascii_lowercase())
                            .or_insert_with(|| (text.into(), common));
                    }
                });
            }
            for (name, entries) in [("cn_en.txt", &mut quanpin), ("cn_en_flypy.txt", &mut flypy)] {
                read_rows(&dir.join(name), false, |text, code| {
                    if printable_ascii(code) {
                        entries.push((code.to_ascii_lowercase(), text.to_owned()));
                    }
                });
                entries.sort_unstable();
                entries.dedup();
            }
        }
        let mut words: Vec<_> = words
            .into_iter()
            .map(|(code, (text, common))| EnglishEntry { code, text, common })
            .collect();
        words.sort_unstable_by(|a, b| a.code.cmp(&b.code));
        Self {
            words,
            quanpin,
            flypy,
        }
    }

    pub(super) fn lookup_english<'a>(
        &'a self,
        raw: &str,
        retention: Option<&'a RetentionEngine>,
    ) -> Option<&'a str> {
        if raw.len() < 2 {
            return None;
        }
        retention
            .and_then(|r| r.query_user_english(raw))
            .or_else(|| {
                let index = self
                    .words
                    .binary_search_by_key(&raw, |entry| entry.code.as_str())
                    .ok()?;
                let text = self.words[index].text.as_str();
                visible(text, retention).then_some(text)
            })
    }

    pub(super) fn complete_english<'a>(
        &'a self,
        raw: &str,
        retention: Option<&'a RetentionEngine>,
        limit: usize,
    ) -> Vec<&'a str> {
        if limit == 0
            || raw.len() < 4
            || !raw.bytes().all(|b| b.is_ascii_lowercase())
            || self.lookup_english(raw, retention).is_some()
        {
            return Vec::new();
        }
        let start = self
            .words
            .partition_point(|entry| entry.code.as_str() < raw);
        let mut matches: Vec<_> = self.words[start..]
            .iter()
            .take_while(|entry| entry.code.starts_with(raw))
            .collect();
        matches.sort_unstable_by_key(|entry| (entry.text.len(), !entry.common, &entry.code));
        let mut seen = HashSet::new();
        matches
            .into_iter()
            .map(|entry| {
                retention
                    .and_then(|r| r.query_user_english(&entry.code))
                    .unwrap_or(&entry.text)
            })
            .filter(|word| visible(word, retention) && seen.insert(*word))
            .take(limit)
            .collect()
    }

    pub(super) fn lookup_cn_en<'a>(
        &'a self,
        raw: &'a str,
        schema: SchemaKind,
        retention: Option<&'a RetentionEngine>,
    ) -> impl Iterator<Item = &'a str> {
        let entries = match schema {
            SchemaKind::RimeIce => &self.quanpin,
            SchemaKind::DoublePinyinFlypy => &self.flypy,
        };
        let start = entries.partition_point(|(code, _)| code.as_str() < raw);
        entries[start..]
            .iter()
            .take_while(move |(code, _)| code == raw)
            .filter_map(move |(_, text)| visible(text, retention).then_some(text.as_str()))
    }
}

pub(super) fn lookup_core_english<'a>(
    raw: &str,
    retention: Option<&'a RetentionEngine>,
) -> Option<&'a str> {
    if raw.len() < 2 || !raw.bytes().all(|b| b.is_ascii_lowercase()) {
        return None;
    }
    retention
        .and_then(|r| r.query_user_english(raw))
        .or_else(|| {
            CORE_ENGLISH
                .iter()
                .copied()
                .find(|word| word.eq_ignore_ascii_case(raw) && visible(word, retention))
        })
}

fn printable_ascii(text: &str) -> bool {
    text.bytes().all(|b| b.is_ascii_graphic() || b == b' ')
}

fn visible(text: &str, retention: Option<&RetentionEngine>) -> bool {
    !retention.is_some_and(|r| r.erased_words.contains(text))
}

fn read_rows(path: &Path, yaml: bool, mut visit: impl FnMut(&str, &str)) {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) => {
            eprintln!("Vitral: failed to read {}: {error}", path.display());
            return;
        }
    };
    let mut lines = contents.lines().enumerate();
    if yaml && !lines.any(|(_, line)| line.trim() == "...") {
        eprintln!(
            "Vitral: missing dictionary header terminator in {}",
            path.display()
        );
        return;
    }
    for (index, line) in lines {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let mut fields = line.split('\t');
        match (fields.next(), fields.next()) {
            (Some(text), Some(code)) if !text.is_empty() && !code.is_empty() => visit(text, code),
            _ => eprintln!(
                "Vitral: invalid dictionary row in {}:{}",
                path.display(),
                index + 1
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_english_preserves_casing_and_completes_prefixes() {
        let assets = crate::imp::polyphone::default_assets_dir_candidates().unwrap();
        let dictionary = EnglishDictionary::open_cached(Some(&assets));
        assert!(dictionary.words.len() > 20_000);
        assert!(Arc::ptr_eq(
            &dictionary,
            &EnglishDictionary::open_cached(Some(&assets))
        ));
        for (code, word) in [
            ("github", "GitHub"),
            ("python", "Python"),
            ("docker", "Docker"),
            ("abandon", "abandon"),
            ("kubernetes", "Kubernetes"),
            ("dejavu", "Déjà vu"),
        ] {
            assert_eq!(dictionary.lookup_english(code, None), Some(word));
        }
        for (code, word) in [("gith", "GitHub"), ("pyth", "Python")] {
            let completed = dictionary.complete_english(code, None, 2);
            assert_eq!(completed.first().copied(), Some(word));
            assert!(completed.len() <= 2);
        }
        for code in ["git", "Pyth", "pyt'", "github"] {
            assert!(dictionary.complete_english(code, None, 2).is_empty());
        }
        let fallback = EnglishDictionary::open_cached(None);
        assert_eq!(fallback.lookup_english("github", None), Some("GitHub"));
        assert_eq!(fallback.lookup_english("kubernetes", None), None);
    }

    #[test]
    fn word_queries_respect_learning_and_erased_words() {
        let assets = crate::imp::polyphone::default_assets_dir_candidates().unwrap();
        let dictionary = EnglishDictionary::open_cached(Some(&assets));
        let mut retention = RetentionEngine::default();
        retention.record_english_casing("PyThon");
        assert_eq!(
            dictionary.lookup_english("python", Some(&retention)),
            Some("PyThon")
        );
        assert_eq!(
            dictionary.complete_english("pyth", Some(&retention), 1),
            ["PyThon"]
        );
        retention
            .erased_words
            .extend(["PyThon", "Python", "X光"].map(str::to_owned));
        assert_eq!(dictionary.lookup_english("python", Some(&retention)), None);
        assert_eq!(
            dictionary.complete_english("pyth", Some(&retention), 1),
            ["pythonic"]
        );
        for (schema, code, other) in [
            (SchemaKind::RimeIce, "xguang", "xgl"),
            (SchemaKind::DoublePinyinFlypy, "xgl", "xguang"),
        ] {
            assert_eq!(
                dictionary
                    .lookup_cn_en(code, schema, None)
                    .collect::<Vec<_>>(),
                ["X光"]
            );
            assert!(dictionary
                .lookup_cn_en(code, schema, Some(&retention))
                .next()
                .is_none());
            assert!(dictionary
                .lookup_cn_en(other, schema, None)
                .next()
                .is_none());
            assert_eq!(
                dictionary
                    .lookup_cn_en("txu", schema, None)
                    .collect::<Vec<_>>(),
                ["T恤"]
            );
        }
    }

    #[test]
    fn english_completions_remove_duplicates_before_applying_the_limit() {
        let dictionary = EnglishDictionary {
            words: [
                ("rustacean", "Rustacean"),
                ("rustlang", "Rust"),
                ("rustlanguage", "Rust"),
                ("rusty", "Rusty"),
            ]
            .map(|(code, text)| EnglishEntry {
                code: code.into(),
                text: text.into(),
                common: true,
            })
            .into(),
            quanpin: Vec::new(),
            flypy: Vec::new(),
        };
        assert_eq!(dictionary.complete_english("rust", None, 2), ["Rust", "Rusty"]);
        let mut retention = RetentionEngine::default();
        retention.erase_word("Rust");
        assert_eq!(
            dictionary.complete_english("rust", Some(&retention), 2),
            ["Rusty", "Rustacean"]
        );
    }
}
