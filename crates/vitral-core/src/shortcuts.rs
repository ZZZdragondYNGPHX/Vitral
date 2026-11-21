use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShortcutAction {
    ToggleLanguage,
    Settings,
    Clipboard,
    Lookup,
    Handwriting,
}

impl ShortcutAction {
    pub const ALL: [Self; 5] = [
        Self::ToggleLanguage,
        Self::Settings,
        Self::Clipboard,
        Self::Lookup,
        Self::Handwriting,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::ToggleLanguage => "切换中英文",
            Self::Settings => "打开设置",
            Self::Clipboard => "打开剪贴板",
            Self::Lookup => "打开查词",
            Self::Handwriting => "打开手写",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShortcutBinding {
    pub action: ShortcutAction,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub text: String,
}

pub fn defaults() -> Vec<ShortcutBinding> {
    ShortcutAction::ALL
        .into_iter()
        .map(|action| ShortcutBinding {
            action,
            key: if action == ShortcutAction::ToggleLanguage {
                "Shift".into()
            } else {
                String::new()
            },
            text: if action == ShortcutAction::Settings {
                "setting".into()
            } else {
                String::new()
            },
        })
        .collect()
}

pub fn normalize_key(key: &str) -> Result<String, String> {
    let key = key.trim();
    if key.is_empty() {
        return Ok(String::new());
    }
    if key.eq_ignore_ascii_case("shift") {
        return Ok("Shift".into());
    }
    let mut parts: Vec<_> = key.split('+').map(str::trim).collect();
    let last = parts.pop().unwrap_or("");
    let mut modifiers = HashSet::new();
    for part in parts {
        let modifier = match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => "Ctrl",
            "alt" => "Alt",
            "shift" => "Shift",
            _ => return Err("快捷键支持 Ctrl、Alt、Shift 组合".into()),
        };
        if !modifiers.insert(modifier) {
            return Err("快捷键修饰键重复".into());
        }
    }
    let upper = last.to_ascii_uppercase();
    let function = upper
        .strip_prefix('F')
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| (1..=12).contains(&n));
    let base = match upper.as_str() {
        "SPACE" => "Space".into(),
        "ENTER" | "RETURN" => "Enter".into(),
        _ if function => format!("F{}", upper[1..].parse::<u8>().unwrap()),
        _ if upper.len() == 1 && upper.bytes().all(|b| b.is_ascii_alphanumeric()) => upper,
        _ => return Err("请使用 Shift、F1–F12，或 Ctrl/Alt 与字母、数字、空格、回车的组合".into()),
    };
    if !function && !modifiers.contains("Ctrl") && !modifiers.contains("Alt") {
        return Err("字母等普通输入键需搭配 Ctrl 或 Alt，避免影响打字".into());
    }
    let mut result: Vec<_> = ["Ctrl", "Alt", "Shift"]
        .into_iter()
        .filter(|m| modifiers.contains(m))
        .collect();
    result.push(&base);
    Ok(result.join("+"))
}

pub fn normalize(bindings: &mut [ShortcutBinding]) -> Result<(), String> {
    let mut actions = HashSet::new();
    let mut keys = HashSet::new();
    for binding in bindings {
        if !actions.insert(binding.action) {
            return Err("每项功能只能配置一条快捷键规则".into());
        }
        binding.key = normalize_key(&binding.key)?;
        if !binding.key.is_empty() && !keys.insert(binding.key.clone()) {
            return Err(format!("快捷键 {} 已绑定其他功能", binding.key));
        }
        binding.text = binding.text.trim().to_ascii_lowercase();
        if !binding.text.bytes().all(|b| b.is_ascii_lowercase()) {
            return Err("功能输入码请使用英文字母；数字保留用于选词".into());
        }
    }
    Ok(())
}
