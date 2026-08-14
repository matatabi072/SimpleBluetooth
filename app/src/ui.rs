// メイン画面
//
// bt スレッドから mpsc で流れてくる Event を毎フレーム drain して
// devices HashMap を更新、それを一覧描画。
//
// 電波強度は Windows が記録した最終値のみ表示する。時系列グラフや帯域図は
// 意図的に持たない: Windows API から「Windows 側で新規に測定された値」と
// 「既に返し続けていたキャッシュ値」を判別する手段がなく、時系列に見せると
// 実測されていない点まで実データのように誤解されるため。詳細は SPEC.md 参照。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use egui::{Color32, RichText, Ui};

use crate::bt::{BtHandle, Command, DeviceKind, Event, RadioState};
use crate::config::Config;
use crate::theme::{self, ThemeChoice};

/// RSSI 表示の「新鮮」しきい値。これより古ければ値の横に (Ns 前) と併記し、
/// バーを半透明化する。あくまで情報表示用: 「電源 OFF」等の推定はしない
/// (Windows の Updated 発火頻度が機器依存で不定なので、更新の疎さから
/// 状態は判断できない)。
const SIGNAL_FRESH_CUTOFF: Duration = Duration::from_secs(15);

pub struct App {
    config: Config,
    devices: HashMap<String, Device>,
    /// 近隣 BLE (未ペアリング含む)。key = BluetoothAddress (48-bit MAC)。
    /// AEP DeviceWatcher 側 (devices) とは別チャネル。BLE 広告直受け。
    nearby: HashMap<u64, NearbyBle>,
    radio: RadioState,
    bt: BtHandle,
    enumerated: bool,
    /// 最後に Added イベントを受け取った時刻。
    /// これから 2.5 秒経ったら EnumerationCompleted 未着火でも「揃った」と見なす
    /// (AEP Watcher は EnumerationCompleted を発火しないので必要な workaround)
    last_added_at: Option<Instant>,
    /// Watcher 稼働開始時刻 (App::new と Rescan で更新)。
    /// ペアリング済み機器が 0 台の場合、Added が 1 度も来ないので last_added_at が
    /// 永遠に None のままになる → enumerated が立たない、を防ぐための総合タイムアウト。
    watcher_started_at: Instant,
    last_dark_applied: Option<bool>,
    view_mode: ViewMode,
    classic_found: Vec<ClassicFound>,
    classic_scan: ClassicScanState,
    /// Classic (ペア済) 機器を「ペアリング済み」タブに表示するか。
    /// **既定 false、Config に永続化しない (起動ごとに OFF に戻る)**。
    /// 変更は設定ウインドウからのみ (メイン画面から 1 クリックで変えられないよう
    /// 意図的にワンクッション置いている)。
    /// 理由: Classic 経由の RSSI は機器・ドライバ実装によっては HCI Read_RSSI の
    /// Golden Range 相対値がそのまま露出することがあり、絶対 dBm として信頼できない。
    /// 誤解を避けるため既定非表示、必要な場合のみユーザーが自己判断で有効化。
    show_classic: bool,
    /// 設定ウインドウの開閉状態 (非永続)。
    show_settings: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ViewMode {
    Paired,
    NearbyBle,
    Classic,
}

pub struct NearbyBle {
    pub address: u64,
    pub name: Option<String>,
    pub rssi: i16,
    pub mfr_company: Option<u16>,
    pub model_hint: Option<String>,
    pub packets: u32,
    pub last_seen: Instant,
}

pub struct ClassicFound {
    pub address: u64,
    pub name: String,
    pub class_of_device: u32,
    pub connected: bool,
    pub paired: bool,
    pub remembered: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ClassicScanState {
    Idle,
    Running,
    Finished(u32),
}

#[derive(Clone)]
pub struct Device {
    #[allow(dead_code)] // HashMap のキー側で持つが、debug 表示用に残す
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
    pub connected: bool,
    pub rssi: Option<i32>,
    /// RSSI を最後に受け取った時刻。表示の age 表記に使う。
    /// Windows が実測した時刻ではなく Watcher イベント受信時刻である点に注意
    /// (Windows API から実測時刻を得る手段は無い)。
    pub last_rssi_at: Option<Instant>,
    #[allow(dead_code)]
    pub address: String,
}

impl Device {
    fn signal_age(&self, now: Instant) -> Option<Duration> {
        self.last_rssi_at.map(|t| now.duration_since(t))
    }
    fn signal_is_fresh(&self, now: Instant) -> bool {
        self.signal_age(now)
            .map(|a| a < SIGNAL_FRESH_CUTOFF)
            .unwrap_or(false)
    }
}

impl App {
    pub fn new(config: Config, bt: BtHandle) -> Self {
        Self {
            config,
            devices: HashMap::new(),
            nearby: HashMap::new(),
            radio: RadioState::Unknown,
            bt,
            enumerated: false,
            last_added_at: None,
            watcher_started_at: Instant::now(),
            last_dark_applied: None,
            view_mode: ViewMode::Paired,
            classic_found: Vec::new(),
            classic_scan: ClassicScanState::Idle,
            show_classic: false,
            show_settings: false,
        }
    }

    fn drain_events(&mut self) {
        let now = Instant::now();
        while let Ok(ev) = self.bt.rx.try_recv() {
            match ev {
                Event::Added { id, name, kind, connected, rssi, address } => {
                    let last_rssi_at = rssi.map(|_| now);
                    self.devices.insert(
                        id.clone(),
                        Device { id, name, kind, connected, rssi, last_rssi_at, address },
                    );
                    self.last_added_at = Some(now);
                }
                Event::Updated { id, connected, rssi } => {
                    if let Some(d) = self.devices.get_mut(&id) {
                        if let Some(c) = connected {
                            d.connected = c;
                        }
                        if let Some(r) = rssi {
                            d.rssi = Some(r);
                            d.last_rssi_at = Some(now);
                        }
                    }
                }
                Event::Removed(id) => {
                    self.devices.remove(&id);
                }
                Event::EnumerationComplete => {
                    self.enumerated = true;
                }
                Event::RescanStarted => {
                    // AEP 側の全機器を捨てる。Added がまた全部届く前提。
                    // 近隣 BLE も明示的にクリアして「更新」の挙動を直感通りにする。
                    // Classic Inquiry 結果も一貫性のためクリア (「更新」= 全リセット)。
                    self.devices.clear();
                    self.nearby.clear();
                    self.classic_found.clear();
                    self.classic_scan = ClassicScanState::Idle;
                    self.enumerated = false;
                    self.last_added_at = None;
                    self.watcher_started_at = now;
                }
                Event::Radio(rs) => {
                    self.radio = rs;
                }
                Event::ClassicInquiryStarted => {
                    self.classic_scan = ClassicScanState::Running;
                    self.classic_found.clear();
                }
                Event::ClassicFound { address, name, class_of_device, connected, paired, remembered } => {
                    self.classic_found.push(ClassicFound {
                        address, name, class_of_device, connected, paired, remembered,
                    });
                }
                Event::ClassicInquiryFinished { total } => {
                    self.classic_scan = ClassicScanState::Finished(total);
                }
                Event::NearbyBleSeen { address, rssi, name, mfr_company, model_hint } => {
                    let e = self.nearby.entry(address).or_insert_with(|| NearbyBle {
                        address, name: None, rssi, mfr_company, model_hint: None,
                        packets: 0, last_seen: now,
                    });
                    e.rssi = rssi;
                    e.packets = e.packets.saturating_add(1);
                    e.last_seen = now;
                    if e.name.is_none() && name.is_some() { e.name = name.clone(); }
                    if e.mfr_company.is_none() && mfr_company.is_some() { e.mfr_company = mfr_company; }
                    if e.model_hint.is_none() && model_hint.is_some() { e.model_hint = model_hint.clone(); }

                    // ペア済み BLE のアドレスと突合して、一致すれば AEP 側の
                    // rssi/last_rssi_at も更新する。BLE 広告経由の値は絶対 dBm
                    // として信頼できる (物理パケット単位の生値)。
                    // (接続中の BLE 機器は広告を出さないのでこの経路には来ない)
                    let v = rssi as i32;
                    for d in self.devices.values_mut() {
                        if d.kind != DeviceKind::Ble { continue; }
                        let Some(paired_addr) = parse_bt_addr(&d.address) else { continue };
                        if paired_addr != address { continue; }
                        d.rssi = Some(v);
                        d.last_rssi_at = Some(now);
                    }
                }
            }
        }
        // 60 秒以上更新のない nearby BLE は捨てる。
        // PC 起動 60 秒以内は checked_sub が None を返すため、その間はスキップ
        // (以前 unwrap_or(now) にしていて起動直後に全 nearby が消える不具合があった)。
        if let Some(cutoff) = now.checked_sub(Duration::from_secs(60)) {
            self.nearby.retain(|_, n| n.last_seen >= cutoff);
        }
        // AEP Watcher の EnumerationCompleted は発火しないので手動で「揃った」判定する。
        //  (a) Added が 2.5 秒来なくなったら揃ったと見なす
        //  (b) Watcher 開始から 4 秒経ったら Added が 1 度も来ていなくても揃ったと見なす
        if !self.enumerated {
            let ready_by_last_added = self
                .last_added_at
                .map(|t| now.duration_since(t) >= Duration::from_millis(2500))
                .unwrap_or(false);
            let ready_by_total_timeout =
                now.duration_since(self.watcher_started_at) >= Duration::from_secs(4);
            if ready_by_last_added || ready_by_total_timeout {
                self.enumerated = true;
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // タイトルバーの Dwm ダーク属性を egui 側のダーク状態に追従させる
        let dark = ctx.style().visuals.dark_mode;
        if self.last_dark_applied != Some(dark) {
            crate::titlebar::apply(frame, dark);
            self.last_dark_applied = Some(dark);
        }

        self.drain_events();

        // 設定ウインドウが開いている間はメイン UI をキーボードごと不活性にする。
        // add_enabled_ui(false) は widget のクリックだけでなく Tab フォーカス移動と
        // Enter による発火も止まるので、スクリムだけでは塞げない Tab 経由の操作を
        // カバーする。視覚的な dim はスクリムで既に付いているので重複しても違和感
        // は少ない。
        let ui_enabled = !self.show_settings;
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_enabled_ui(ui_enabled, |ui| self.top_bar(ui, ctx));
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_enabled_ui(ui_enabled, |ui| self.device_list(ui));
        });
        self.settings_window(ctx);

        // AEP Updated / BLE 広告受信でイベントが来た瞬間に repaint 要求は
        // 走るので、常時再描画は不要。それでも age 表記を秒単位で進めるため
        // 500ms 毎に再描画予約。
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

impl App {
    fn top_bar(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let total = self.devices.len();
        ui.horizontal(|ui| {
            ui.strong("SimpleBluetooth");
            ui.separator();
            // 「テーマ:」ラベルは strong で色をハッキリさせる (ライト/ダーク両テーマで
            // strong_text_color が使われる)
            ui.strong("テーマ:");
            let before = self.config.theme;
            egui::ComboBox::from_id_salt("theme_selector")
                .selected_text(
                    RichText::new(self.config.theme.label())
                        .color(ui.visuals().strong_text_color()),
                )
                .show_ui(ui, |ui| {
                    for c in [ThemeChoice::System, ThemeChoice::Dark, ThemeChoice::Light] {
                        ui.selectable_value(&mut self.config.theme, c, c.label());
                    }
                });
            if self.config.theme != before {
                theme::apply(ctx, self.config.theme);
                self.config.save();
            }
            ui.separator();
            // Bluetooth ラジオ状態 (Windows API の RadioKind::Bluetooth を抜粋)。
            // ラベルは strong、状態はテーマに合わせた色付き。
            ui.strong("Bluetooth:");
            let (radio_str, radio_color) = radio_label_colored(self.radio, ui.visuals().dark_mode);
            ui.label(RichText::new(radio_str).strong().color(radio_color));
            ui.separator();
            // 台数表示: weak をやめて通常コントラストに
            let status = if self.enumerated {
                format!("{} 台", total)
            } else {
                format!("読込中... {}", total)
            };
            ui.label(RichText::new(status).color(ui.visuals().strong_text_color()));
            ui.separator();
            // AEP Watcher は起動後に新規ペアリングされた機器を Added で通知しないので、
            // 手動 rescan を用意する。ペアリング後にこのボタンを押して一覧を再取得。
            if ui.button("更新").on_hover_text("Watcher を再作成して一覧を最新化 (新規ペアリング分を反映)").clicked() {
                let _ = self.bt.cmd_tx.send(Command::Rescan);
            }
            ui.separator();
            if ui.button("設定").on_hover_text("表示に関する詳細設定を開く").clicked() {
                self.show_settings = true;
            }
        });
    }

    fn device_list(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.view_mode, ViewMode::Paired, "ペアリング済み");
            ui.selectable_value(&mut self.view_mode, ViewMode::NearbyBle, "近くの BLE");
            ui.selectable_value(&mut self.view_mode, ViewMode::Classic, "Classic 探索");
        });
        ui.add_space(4.0);

        match self.view_mode {
            ViewMode::Paired => self.paired_list(ui),
            ViewMode::NearbyBle => self.nearby_list(ui),
            ViewMode::Classic => self.classic_list(ui),
        }
    }

    fn paired_list(&mut self, ui: &mut Ui) {
        // 常時表示の注意書き: 値の性質を誤解しないための保険。
        // 目立つように枠線 + 警告色 + サイズ大きめ。
        let warn = ui.visuals().warn_fg_color;
        egui::Frame::none()
            .stroke(egui::Stroke::new(1.0, warn))
            .inner_margin(egui::Margin::same(8.0))
            .rounding(4.0)
            .show(ui, |ui| {
                ui.label(
                    RichText::new(
                        "⚠ 電波強度は Windows が記録した最終値です。リアルタイム測定ではなく、\
                         更新間隔は Windows 任せ (数秒〜数分)。相対的な変化を眺める用途を推奨します。"
                    )
                    .size(15.0)
                    .color(warn),
                );
            });
        ui.add_space(6.0);

        let show_classic = self.show_classic;
        let mut ids: Vec<String> = self.devices.iter()
            .filter(|(_, d)| show_classic || d.kind == DeviceKind::Ble)
            .map(|(id, _)| id.clone())
            .collect();

        if ids.is_empty() && self.enumerated {
            if show_classic {
                ui.label("ペアリング済み機器がありません。");
            } else {
                ui.label("ペアリング済み BLE 機器がありません。(Classic 機器は「設定」から表示)");
            }
            return;
        }

        let now = Instant::now();

        // 表示順:
        //   1. BLE を先、Classic を後 (Classic の RSSI は絶対値として比較不可
        //      = Golden Range 相対値の可能性があるため強い順ソートに混ぜない)
        //   2. 同種内は接続中を上
        //   3. BLE は RSSI 強い順 (未取得は末尾)、Classic は名前順
        ids.sort_by(|a, b| {
            let da = &self.devices[a];
            let db = &self.devices[b];
            // BLE (Ble) と Classic (Classic) — Ord は enum 宣言順 (Classic < Ble) に
            // 依存するので、明示的に BLE 優先にする
            let a_is_ble = da.kind == DeviceKind::Ble;
            let b_is_ble = db.kind == DeviceKind::Ble;
            b_is_ble
                .cmp(&a_is_ble)
                .then_with(|| db.connected.cmp(&da.connected))
                .then_with(|| {
                    if a_is_ble {
                        db.rssi.unwrap_or(i32::MIN).cmp(&da.rssi.unwrap_or(i32::MIN))
                    } else {
                        std::cmp::Ordering::Equal
                    }
                })
                .then_with(|| da.name.cmp(&db.name))
        });

        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("devices")
                .num_columns(5)
                .spacing([12.0, 6.0])
                .striped(true)
                .show(ui, |ui| {
                    ui.strong("種別");
                    ui.strong("RSSI");
                    ui.strong("信号");
                    ui.strong("状態");
                    ui.strong("名前");
                    ui.end_row();

                    for id in &ids {
                        let d = &self.devices[id];
                        let fresh = d.signal_is_fresh(now);
                        let age = d.signal_age(now);
                        ui.label(kind_label(d.kind));
                        ui.label(rssi_text_with_age(d.rssi, age));
                        rssi_bar(ui, d.rssi, !fresh);
                        ui.label(if d.connected { "接続中" } else { "切断" });
                        ui.label(RichText::new(&d.name).strong());
                        ui.end_row();
                    }
                });
        });
    }

    fn nearby_list(&mut self, ui: &mut Ui) {
        ui.weak(format!(
            "BLE 広告を直接受信 (500ms スロットル)。現在 {} 台。値は絶対 dBm。",
            self.nearby.len()
        ));
        ui.add_space(4.0);

        if self.nearby.is_empty() {
            ui.label("近くの BLE 広告はまだ観測されていません...");
            return;
        }

        // ソート: 名前 or model_hint が付いているものを上、その中で RSSI 強い順
        let mut list: Vec<&NearbyBle> = self.nearby.values().collect();
        list.sort_by(|a, b| {
            let a_known = a.name.is_some() || a.model_hint.is_some();
            let b_known = b.name.is_some() || b.model_hint.is_some();
            b_known
                .cmp(&a_known)
                .then_with(|| b.rssi.cmp(&a.rssi))
        });

        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("nearby")
                .num_columns(5)
                .spacing([12.0, 6.0])
                .striped(true)
                .show(ui, |ui| {
                    ui.strong("RSSI");
                    ui.strong("信号");
                    ui.strong("pkt");
                    ui.strong("アドレス");
                    ui.strong("識別");
                    ui.end_row();

                    for n in list {
                        ui.label(format!("{} dBm", n.rssi));
                        rssi_bar(ui, Some(n.rssi as i32), false);
                        ui.label(format!("{}", n.packets));
                        ui.label(RichText::new(fmt_bt_addr(n.address))
                            .family(egui::FontFamily::Monospace));
                        let label = if let Some(m) = &n.model_hint {
                            RichText::new(m).strong().color(Color32::from_rgb(120, 220, 160))
                        } else if let Some(nm) = &n.name {
                            RichText::new(nm).strong()
                        } else if let Some(cid) = n.mfr_company {
                            RichText::new(format!("Company 0x{:04X}", cid)).weak()
                        } else {
                            RichText::new("(unnamed)").weak()
                        };
                        ui.label(label);
                        ui.end_row();
                    }
                });
        });
    }

    fn classic_list(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let scanning = matches!(self.classic_scan, ClassicScanState::Running);
            let btn = ui.add_enabled(!scanning, egui::Button::new(
                if scanning { "スキャン中...(~8秒)" } else { "スキャン開始 (約 8 秒)" }
            ));
            if btn.clicked() {
                let _ = self.bt.cmd_tx.send(Command::StartClassicInquiry);
            }
            ui.separator();
            match self.classic_scan {
                ClassicScanState::Idle => {
                    ui.weak("(未実行。ボタンで開始)");
                }
                ClassicScanState::Running => {
                    ui.weak(format!("スキャン中… ({} 台検出)", self.classic_found.len()));
                }
                ClassicScanState::Finished(total) => {
                    ui.weak(format!("完了: {} 台", total));
                }
            }
        });
        ui.weak("Classic Inquiry はペアリングモードの機器を発見します (RSSI は取得不可)。");
        ui.add_space(4.0);

        if self.classic_found.is_empty() {
            ui.label("(まだ結果がありません。上のボタンで Inquiry を発行してください)");
            return;
        }

        // ソート: 接続中 → 未ペア → ペア済み、その中で名前あり優先
        let mut list: Vec<&ClassicFound> = self.classic_found.iter().collect();
        list.sort_by(|a, b| {
            b.connected
                .cmp(&a.connected)
                .then_with(|| a.paired.cmp(&b.paired)) // 未ペア (false) が先
                .then_with(|| (!b.name.is_empty()).cmp(&(!a.name.is_empty())))
        });

        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("classic_found")
                .num_columns(5)
                .spacing([12.0, 6.0])
                .striped(true)
                .show(ui, |ui| {
                    ui.strong("アドレス");
                    ui.strong("名前");
                    ui.strong("Class");
                    ui.strong("状態");
                    ui.strong("フラグ");
                    ui.end_row();

                    for c in list {
                        ui.label(RichText::new(fmt_bt_addr(c.address))
                            .family(egui::FontFamily::Monospace));
                        let name = if c.name.is_empty() { "(no name)".into() } else { c.name.clone() };
                        ui.label(RichText::new(name).strong());
                        ui.label(RichText::new(cod_label(c.class_of_device))
                            .weak());
                        ui.label(if c.connected { "接続中" } else if c.paired { "切断 (ペア済)" } else { "未ペア" });
                        let mut flags = Vec::new();
                        if c.paired { flags.push("paired"); }
                        if c.remembered { flags.push("remembered"); }
                        if c.connected { flags.push("connected"); }
                        ui.weak(flags.join(","));
                        ui.end_row();
                    }
                });
        });
    }

    /// 設定ウインドウ (モーダル)。開いている間はメイン画面のクリックを吸収する。
    /// egui::Window 自体はモーダル属性を持たないので、Order::Middle に半透明の
    /// スクリム Area を敷いてクリックを飲み込み、Window を Order::Foreground に
    /// 置いて手前に出す構成。ESC / × / スクリム外クリックで閉じる。
    /// メイン画面からは「設定」ボタンでのみ開く (Classic 表示切替が偶発的に
    /// 変わらないようワンクッション置く狙い)。
    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        // ESC で閉じる
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.show_settings = false;
            return;
        }

        // スクリム (半透明下地) — メイン UI へのクリックを吸収する。
        // Order::Middle は CentralPanel/TopBottomPanel (Order::Background 相当) より
        // 手前、後述の Window (Order::Foreground) より奥。
        let screen = ctx.screen_rect();
        egui::Area::new(egui::Id::new("__settings_scrim"))
            .fixed_pos(screen.min)
            .order(egui::Order::Middle)
            .interactable(true)
            .show(ctx, |ui| {
                ui.painter().rect_filled(
                    screen,
                    0.0,
                    egui::Color32::from_black_alpha(140),
                );
                // 全画面 rect を click_and_drag で受けてメイン画面へ操作を通さない
                ui.interact(
                    screen,
                    egui::Id::new("__settings_scrim_sink"),
                    egui::Sense::click_and_drag(),
                );
            });

        let mut open = true;
        egui::Window::new("設定")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(460.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.heading("表示");
                ui.add_space(6.0);

                ui.checkbox(
                    &mut self.show_classic,
                    RichText::new("Classic 機器も「ペアリング済み」タブに表示する").size(14.0),
                );
                ui.weak("この設定は保存されません (起動ごとに OFF に戻ります)。");
                ui.add_space(8.0);

                let warn = ui.visuals().warn_fg_color;
                egui::Frame::none()
                    .stroke(egui::Stroke::new(1.0, warn))
                    .inner_margin(egui::Margin::same(10.0))
                    .rounding(4.0)
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new("⚠ Classic 機器の電波強度について")
                                .strong()
                                .size(14.0)
                                .color(warn),
                        );
                        ui.add_space(4.0);
                        ui.label(RichText::new(
                            "Classic 機器 (ヘッドセット等) の電波強度は、機種・ドライバに\
                             よっては絶対 dBm ではなく Bluetooth 規格の Golden Range 相対値が\
                             そのまま露出する場合があります。機器間の絶対比較には使えません。"
                        ).size(13.0));
                        ui.add_space(4.0);
                        ui.label(RichText::new(
                            "機種によっては本来 -30 dBm 台のはずの機器が \
                             0 dBm 付近の異常に高い値として表示されることがあります。"
                        ).weak());
                        ui.add_space(4.0);
                        ui.label(RichText::new(
                            "有効化する場合は参考値として扱ってください。"
                        ).size(13.0));
                    });
            });
        if !open {
            self.show_settings = false;
        }
    }
}

/// AEP DeviceAddress プロパティ (例: "aa:bb:cc:dd:ee:ff" や "AABBCCDDEEFF") を
/// 48-bit MAC の u64 に変換する。区切りは無視して 16 進 12 文字を拾う。
fn parse_bt_addr(s: &str) -> Option<u64> {
    let cleaned: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if cleaned.len() != 12 { return None; }
    u64::from_str_radix(&cleaned, 16).ok()
}

/// Bluetooth Class of Device (24-bit) → 主機種名。
/// Major Device Class は bits 12-8。詳細は Bluetooth Assigned Numbers 参照。
fn cod_label(cod: u32) -> &'static str {
    let major = (cod >> 8) & 0x1F;
    match major {
        0x01 => "Computer",
        0x02 => "Phone",
        0x03 => "LAN/Access Point",
        0x04 => "Audio/Video",
        0x05 => "Peripheral (Mouse/KB)",
        0x06 => "Imaging (Printer)",
        0x07 => "Wearable",
        0x08 => "Toy",
        0x09 => "Health",
        _ => "Uncategorized",
    }
}

fn fmt_bt_addr(addr: u64) -> String {
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        (addr >> 40) & 0xff,
        (addr >> 32) & 0xff,
        (addr >> 24) & 0xff,
        (addr >> 16) & 0xff,
        (addr >> 8) & 0xff,
        addr & 0xff
    )
}

fn kind_label(k: DeviceKind) -> &'static str {
    match k {
        DeviceKind::Classic => "Classic",
        DeviceKind::Ble => "BLE",
    }
}

#[allow(dead_code)] // 色付きバージョンに置き換えたが将来の非色情報用途で残す
fn radio_label(r: RadioState) -> &'static str {
    match r {
        RadioState::On => "ON",
        RadioState::Off => "OFF",
        RadioState::Disabled => "無効",
        RadioState::Absent => "無し",
        RadioState::Unknown => "?",
    }
}

/// Radio 状態を「テキスト + 色」で返す。ダーク/ライトで彩度を調整する。
/// ON = 緑、OFF/無効/無し = 赤系、Unknown = 中間色。
fn radio_label_colored(r: RadioState, dark: bool) -> (&'static str, Color32) {
    let ok = if dark {
        Color32::from_rgb(120, 220, 140)   // ダーク背景に映える明るめの緑
    } else {
        Color32::from_rgb(30, 130, 60)     // ライト背景で読める暗めの緑
    };
    let bad = if dark {
        Color32::from_rgb(240, 130, 130)
    } else {
        Color32::from_rgb(180, 40, 40)
    };
    let neutral = if dark {
        Color32::from_rgb(200, 200, 200)
    } else {
        Color32::from_rgb(80, 80, 80)
    };
    match r {
        RadioState::On => ("ON", ok),
        RadioState::Off => ("OFF", bad),
        RadioState::Disabled => ("無効", bad),
        RadioState::Absent => ("無し", bad),
        RadioState::Unknown => ("?", neutral),
    }
}

/// RSSI 値表示。5 秒以上経っていれば経過秒を併記する。
fn rssi_text_with_age(rssi: Option<i32>, age: Option<Duration>) -> String {
    match rssi {
        Some(v) => {
            let secs = age.map(|a| a.as_secs()).unwrap_or(0);
            if secs >= 5 {
                format!("{} dBm ({}s 前)", v, secs)
            } else {
                format!("{} dBm", v)
            }
        }
        None => "-- dBm".into(),
    }
}

/// RSSI 横バー。dBm を赤〜黄〜緑にマップ (0 → 満タン緑, -100 → 空 赤)
/// dim=true のときは半透明化 (SIGNAL_FRESH_CUTOFF を過ぎた古い値の視覚ヒント)
fn rssi_bar(ui: &mut Ui, rssi: Option<i32>, dim: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(100.0, 14.0), egui::Sense::hover());
    let painter = ui.painter();
    let bg = ui.visuals().extreme_bg_color;
    painter.rect_filled(rect, 2.0, bg);
    if let Some(v) = rssi {
        let ratio = ((v as f32 + 100.0) / 100.0).clamp(0.0, 1.0);
        let mut color = signal_color(ratio);
        if dim {
            color = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 100);
        }
        let mut fill = rect;
        fill.set_width(rect.width() * ratio);
        painter.rect_filled(fill, 2.0, color);
    }
    painter.rect_stroke(
        rect,
        2.0,
        egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
    );
}

fn signal_color(ratio: f32) -> Color32 {
    if ratio < 0.5 {
        let t = ratio / 0.5;
        lerp_color(Color32::from_rgb(200, 60, 60), Color32::from_rgb(210, 180, 40), t)
    } else {
        let t = (ratio - 0.5) / 0.5;
        lerp_color(
            Color32::from_rgb(210, 180, 40),
            Color32::from_rgb(60, 170, 80),
            t,
        )
    }
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let f = |x: u8, y: u8| ((x as f32) * (1.0 - t) + (y as f32) * t) as u8;
    Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
}
