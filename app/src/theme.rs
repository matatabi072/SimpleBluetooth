// テーマ 3 択 ([[theme-selection-three-options]] 準拠)
//
// System = OS の Apps テーマに追従 (デフォルト)
// Dark   = 常にダーク
// Light  = 常にライト

use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    System,
    Dark,
    Light,
}

impl Default for ThemeChoice {
    fn default() -> Self {
        ThemeChoice::System
    }
}

impl ThemeChoice {
    pub fn label(self) -> &'static str {
        match self {
            ThemeChoice::System => "OS 準拠",
            ThemeChoice::Dark => "ダーク",
            ThemeChoice::Light => "ライト",
        }
    }
}

/// egui コンテキストに現在の選択を適用する。
/// System の場合はレジストリを読んで OS 現状に追従。
pub fn apply(ctx: &egui::Context, choice: ThemeChoice) {
    let visuals = match choice {
        ThemeChoice::Dark => egui::Visuals::dark(),
        ThemeChoice::Light => egui::Visuals::light(),
        ThemeChoice::System => {
            if system_prefers_dark() {
                egui::Visuals::dark()
            } else {
                egui::Visuals::light()
            }
        }
    };
    ctx.set_visuals(visuals);
}

/// Windows レジストリの AppsUseLightTheme を読む。1 = ライト, 0 = ダーク。
/// 取れない場合 (非 Windows / 未設定) は false (ライトとみなす)。
///
/// **注意**: 以前は `reg.exe` を spawn する実装だったが、サブプロセス起動で
/// 数百 ms かかって「OS 準拠」選択時に体感的なラグが出ていた。windows crate の
/// RegGetValueW を直呼びに切り替えて μs オーダーに短縮。
fn system_prefers_dark() -> bool {
    #[cfg(windows)]
    unsafe {
        use windows::core::w;
        use windows::Win32::Foundation::ERROR_SUCCESS;
        use windows::Win32::System::Registry::{
            RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD,
        };

        let mut value: u32 = 0;
        let mut size: u32 = std::mem::size_of::<u32>() as u32;
        let status = RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut size),
        );
        if status == ERROR_SUCCESS {
            return value == 0;
        }
    }
    false
}
