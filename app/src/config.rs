// %APPDATA%\SimpleBluetooth\config.toml の読み書き
//
// 現状はテーマ設定のみ。将来の設定はここに追加していく。
// レジストリは使わない ([[windows-app-data-layout]] 準拠)

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::theme::ThemeChoice;

#[derive(Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub theme: ThemeChoice,
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        std::env::var_os("APPDATA")
            .map(|v| PathBuf::from(v).join("SimpleBluetooth").join("config.toml"))
    }

    pub fn load() -> Self {
        let Some(p) = Self::path() else {
            return Self::default();
        };
        let Ok(s) = std::fs::read_to_string(&p) else {
            return Self::default();
        };
        toml::from_str(&s).unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(p) = Self::path() else { return };
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(s) = toml::to_string_pretty(self) {
            let _ = std::fs::write(p, s);
        }
    }
}
