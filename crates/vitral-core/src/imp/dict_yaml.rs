use crate::error::VitralError;
use crate::imp::is_valid_pinyin_code;
use crate::imp::xiaohe::normalize_syllable;

#[derive(Debug, Clone)]
pub struct RawDictYamlEntry {
    pub text: String,
    pub syllables: Option<Vec<String>>,
    pub weight: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColKind {
    Text,
    Code,
    Weight,
    Stem,
    Ignore,
}

#[derive(Debug, Clone)]
pub struct ParsedDictYaml {
    pub name: Option<String>,
    pub sort_by_weight: bool,
    pub entries: Vec<RawDictYamlEntry>,
    pub skipped_count: usize,
    pub warnings: Vec<String>,
}

/// Parse Rime `.dict.yaml` (`--- ... ...` frontmatter + TSV body).
pub fn parse_dict_yaml(content: &str) -> Result<ParsedDictYaml, VitralError> {
    let content = content.strip_prefix('\u{FEFF}').unwrap_or(content);
    let mut lines = content.lines().enumerate().peekable();
    let mut found_start = false;
    let mut found_end = false;
    let mut name = None;
    let mut sort_by_weight = false;
    let mut columns: Vec<ColKind> = vec![ColKind::Text, ColKind::Code, ColKind::Weight];
    let mut in_columns = false;
    let mut parsed_columns: Vec<ColKind> = Vec::new();

    for (_, raw_line) in lines.by_ref() {
        let line = raw_line.trim();
        if !found_start {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line == "---" {
                found_start = true;
                continue;
            }
            return Err(VitralError::Parse(
                "missing YAML frontmatter start '---' in .dict.yaml".into(),
            ));
        }

        if line == "..." {
            found_end = true;
            break;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if in_columns {
            if let Some(item) = line.strip_prefix('-') {
                let col_name = item.trim();
                parsed_columns.push(parse_col_kind(col_name));
                continue;
            } else {
                if !parsed_columns.is_empty() {
                    columns = parsed_columns.clone();
                }
                in_columns = false;
            }
        }

        if let Some(val) = line.strip_prefix("name:") {
            name = Some(val.trim().trim_matches('"').trim_matches('\'').to_string());
        } else if let Some(val) = line.strip_prefix("sort:") {
            let s = val.trim().trim_matches('"').trim_matches('\'');
            sort_by_weight = s == "by_weight";
        } else if let Some(val) = line.strip_prefix("columns:") {
            let v = val.trim();
            if v.starts_with('[') && v.ends_with(']') {
                let inner = &v[1..v.len() - 1];
                let cols: Vec<ColKind> = inner
                    .split(',')
                    .map(|s| parse_col_kind(s.trim()))
                    .collect();
                if !cols.is_empty() {
                    columns = cols;
                }
            } else if v.is_empty() {
                in_columns = true;
                parsed_columns.clear();
            }
        }
    }

    if in_columns && !parsed_columns.is_empty() {
        columns = parsed_columns;
    }

    if !found_start || !found_end {
        return Err(VitralError::Parse(
            "missing '---' / '...' YAML frontmatter delimiters in .dict.yaml".into(),
        ));
    }

    let mut entries = Vec::new();
    let mut skipped_count = 0usize;
    let mut warnings = Vec::new();

    for (line_idx, raw_line) in lines {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = raw_line.split('\t').collect();
        let mut text = String::new();
        let mut code_str: Option<String> = None;
        let mut weight: u32 = 100;
        let mut bad_weight = false;

        for (i, col_kind) in columns.iter().enumerate() {
            let Some(val) = parts.get(i).map(|s| s.trim()) else {
                continue;
            };
            match col_kind {
                ColKind::Text => text = val.to_string(),
                ColKind::Code => {
                    if !val.is_empty() {
                        code_str = Some(val.to_string());
                    }
                }
                ColKind::Weight => {
                    if !val.is_empty() {
                        match val.parse::<u32>() {
                            Ok(w) => weight = w,
                            Err(_) => {
                                bad_weight = true;
                            }
                        }
                    }
                }
                ColKind::Stem | ColKind::Ignore => {}
            }
        }

        let invalid_code = code_str
            .as_deref()
            .is_some_and(|c| !is_valid_pinyin_code(c));
        if text.is_empty() || bad_weight || invalid_code {
            skipped_count += 1;
            warnings.push(format!("dict.yaml line {}: skipped malformed row", line_idx + 1));
            continue;
        }

        let syllables = code_str.map(|c| {
            c.split_whitespace()
                .map(normalize_syllable)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        });

        entries.push(RawDictYamlEntry {
            text,
            syllables,
            weight,
        });
    }

    if sort_by_weight {
        entries.sort_by_key(|a| std::cmp::Reverse(a.weight));
    }

    Ok(ParsedDictYaml {
        name,
        sort_by_weight,
        entries,
        skipped_count,
        warnings,
    })
}

fn parse_col_kind(name: &str) -> ColKind {
    let unquoted = name.trim().trim_matches('"').trim_matches('\'').trim();
    match unquoted {
        "text" => ColKind::Text,
        "code" => ColKind::Code,
        "weight" => ColKind::Weight,
        "stem" => ColKind::Stem,
        _ => ColKind::Ignore,
    }
}
