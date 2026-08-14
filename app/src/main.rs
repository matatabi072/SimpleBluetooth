// SimpleBluetooth Ver 0.1 スケルトン
//
// 現状:
//   - eframe/egui による GUI
//   - テーマ 3 択 (OS 準拠 / ダーク / ライト)
//   - Config を %APPDATA%\SimpleBluetooth\config.toml に保存
//   - Device 一覧はまだダミー (次段で rssi_probe の実装を配線する)

// 常時 GUI サブシステム ([[windows-gui-app-pitfalls]])。
// debug でコンソールを出す cfg_attr パターンは、黒い窓が本体と並んで
// 出っぱなしになり利用中に邪魔なので採用しない。
// panic は将来ログ + MessageBox に配線する予定。
#![windows_subsystem = "windows"]

mod bt;
mod config;
mod theme;
mod titlebar;
mod ui;

use anyhow::Result;

fn main() -> Result<()> {
    let config = config::Config::load();
    let initial_theme = config.theme;

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([780.0, 720.0])
            .with_min_inner_size([560.0, 520.0])
            .with_title("SimpleBluetooth"),
        ..Default::default()
    };

    eframe::run_native(
        "SimpleBluetooth",
        native_options,
        Box::new(move |cc| {
            setup_fonts(&cc.egui_ctx);
            theme::apply(&cc.egui_ctx, initial_theme);
            let handle = bt::spawn(cc.egui_ctx.clone());
            Ok(Box::new(ui::App::new(config, handle)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))?;

    Ok(())
}

fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    // 日本語表示用フォントを Windows システムから読み込む。
    // Yu Gothic / Meiryo / MS Gothic の順で試す。全て TTC (Truetype Collection)。
    let candidates = [
        r"C:\Windows\Fonts\YuGothM.ttc",
        r"C:\Windows\Fonts\YuGothR.ttc",
        r"C:\Windows\Fonts\meiryo.ttc",
        r"C:\Windows\Fonts\msgothic.ttc",
    ];
    for path in candidates {
        if let Ok(data) = std::fs::read(path) {
            let name = "jp".to_owned();
            fonts
                .font_data
                .insert(name.clone(), egui::FontData::from_owned(data));
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, name.clone());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push(name);
            break;
        }
    }
    ctx.set_fonts(fonts);
}
