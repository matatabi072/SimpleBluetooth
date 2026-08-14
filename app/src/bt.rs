// Bluetooth デバイスの取得をバックグラウンドスレッドで回し、UI に mpsc で流す。
//
// poc/rssi_probe および poc/rssi_trend の実装を踏襲。既知の制約:
//   * DeviceWatcher は Added/Updated/Removed/EnumerationCompleted 全て登録必須
//     ([[windows-device-watcher-handlers]])
//   * AEP.SignalStrength = 0 dBm はセンチネル (未測定) → 表示から除外
//   * Radio priming: Radio::GetRadiosAsync を先に叩かないと Watcher が発火しない
//     (実測、rssi_poll 参照)

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use windows::Devices::Enumeration::{
    DeviceInformation, DeviceInformationKind, DeviceInformationUpdate,
};
use windows::Devices::Radios::{Radio, RadioKind, RadioState as WinRadioState};
use windows::Foundation::{IPropertyValue, TypedEventHandler};
use windows_collections::{IIterable, IIterable_Impl, IIterator, IIterator_Impl, IMapView};
use windows_core::{implement, IInspectable, Interface, HSTRING};

const PROP_SIGNAL: &str = "System.Devices.Aep.SignalStrength";
const PROP_CONNECTED: &str = "System.Devices.Aep.IsConnected";
const PROP_ADDRESS: &str = "System.Devices.Aep.DeviceAddress";
const PROTOCOL_BT_CLASSIC: &str = "{e0cbf06c-cd8b-4647-bb8a-263b43f0f974}";
const PROTOCOL_BT_LE: &str = "{bb7bb05e-5972-42b5-94fc-76eaa7084d49}";

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    Classic,
    Ble,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RadioState {
    On,
    Off,
    Disabled,
    Unknown,
    Absent,
}

/// UI 側に流すイベント。UI スレッド側で HashMap<id, Device> を更新する。
#[derive(Debug)]
pub enum Event {
    Added {
        id: String,
        name: String,
        kind: DeviceKind,
        connected: bool,
        rssi: Option<i32>,
        address: String,
    },
    Updated {
        id: String,
        connected: Option<bool>,
        rssi: Option<i32>,
    },
    Removed(String),
    EnumerationComplete,
    /// Rescan コマンドで Watcher を作り直す直前に発火。
    /// UI 側は既存の devices/history を全てクリアし、Added がまた全部届くのを待つ。
    RescanStarted,
    Radio(RadioState),

    /// 近くの (未ペア含む) BLE 広告 1 パケット観測。
    /// address は BluetoothAddress (48-bit MAC を u64 に詰めたもの)。
    /// mfr_company: Manufacturer Data の Company ID (最初の 1 つ)。
    /// model_hint: SwitchBot 等の判別結果 (poc/switchbot_probe と同ロジック)。
    NearbyBleSeen {
        address: u64,
        rssi: i16,
        name: Option<String>,
        mfr_company: Option<u16>,
        model_hint: Option<String>,
    },
    /// Classic Inquiry の開始通知 (UI が「スキャン中」を表示するため)
    ClassicInquiryStarted,
    /// Classic Inquiry の 1 機器発見
    ClassicFound {
        address: u64,
        name: String,
        class_of_device: u32,
        connected: bool,
        paired: bool,
        remembered: bool,
    },
    /// Classic Inquiry 完了通知
    ClassicInquiryFinished { total: u32 },
}

/// UI 側から bt スレッドへの制御コマンド。
#[derive(Debug)]
pub enum Command {
    /// AEP Watcher を作り直し (新規ペアリング機器を拾うため)
    Rescan,
    /// Bluetooth Classic Inquiry (~8秒ブロック、専用スレッドで実行)
    StartClassicInquiry,
}

pub struct BtHandle {
    pub rx: Receiver<Event>,
    pub cmd_tx: Sender<Command>,
}

pub fn spawn(ctx: egui::Context) -> BtHandle {
    let (tx, rx) = std::sync::mpsc::channel::<Event>();
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Command>();
    thread::spawn(move || {
        if let Err(e) = run(tx, ctx, cmd_rx) {
            eprintln!("[bt thread] fatal: {e:?}");
        }
    });
    BtHandle { rx, cmd_tx }
}

fn run(tx: Sender<Event>, ctx: egui::Context, cmd_rx: Receiver<Command>) -> Result<()> {
    // ---- Radio priming (WinRT を MTA で起こす。詳細は rssi_poll SPEC) ----
    let radios = pollster::block_on(Radio::GetRadiosAsync()?)?;
    let radio_state = extract_bt_radio_state(&radios);
    let _ = tx.send(Event::Radio(radio_state));

    // 起動時に一度だけ Watcher を作成。Rescan コマンドが来たら Stop → 破棄 → 再作成する。
    let mut watcher = build_watcher(&tx, &ctx)?;
    watcher.Start()?;

    // 並行して BLE 広告スキャンも起動 (近くの未ペア機器を拾う)。
    // poc/ble_advertisement + poc/switchbot_probe と同ロジック。
    let _ble_watcher = build_ble_advertisement_watcher(&tx, &ctx)?;

    loop {
        // 10 秒待ちながら Command を検査。Rescan が来たら Watcher を作り直す。
        match cmd_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Command::Rescan) => {
                let _ = watcher.Stop();
                let _ = tx.send(Event::RescanStarted);
                ctx.request_repaint();
                watcher = build_watcher(&tx, &ctx)?;
                watcher.Start()?;
            }
            Ok(Command::StartClassicInquiry) => {
                // Win32 BluetoothFindFirstDevice は約 8 秒ブロックするので
                // 別スレッドで実行。結果は tx 経由で UI に流す。
                let tx2 = tx.clone();
                let ctx2 = ctx.clone();
                thread::spawn(move || {
                    let _ = tx2.send(Event::ClassicInquiryStarted);
                    ctx2.request_repaint();
                    let total = run_classic_inquiry(&tx2, &ctx2);
                    let _ = tx2.send(Event::ClassicInquiryFinished { total });
                    ctx2.request_repaint();
                });
            }
            Err(RecvTimeoutError::Timeout) => { /* 10 秒経過 → Radio 状態を更新 */ }
            Err(RecvTimeoutError::Disconnected) => {
                // App drop (プロセス終了) で cmd_tx が落ちた。ビジーループ回避のため終了。
                return Ok(());
            }
        }
        // Radio 状態のポーリングは失敗してもスレッドを殺さない。
        // 一時的な WinRT エラーで bt スレッドごと死ぬと AEP/BLE/Classic 全部止まる。
        if let Ok(op) = Radio::GetRadiosAsync() {
            if let Ok(rs) = pollster::block_on(op) {
                let state = extract_bt_radio_state(&rs);
                let _ = tx.send(Event::Radio(state));
                ctx.request_repaint();
            }
        }
    }
}

/// AEP Watcher を作って 4 ハンドラを登録した状態で返す (Start はしない)。
fn build_watcher(
    tx: &Sender<Event>,
    ctx: &egui::Context,
) -> Result<windows::Devices::Enumeration::DeviceWatcher> {
    let aqs = format!(
        "(System.Devices.Aep.ProtocolId:=\"{}\" OR System.Devices.Aep.ProtocolId:=\"{}\") \
         AND System.Devices.Aep.IsPaired:=System.StructuredQueryType.Boolean#True",
        PROTOCOL_BT_CLASSIC, PROTOCOL_BT_LE
    );
    let aqs_hs = HSTRING::from(aqs);
    let props: Vec<HSTRING> = vec![
        HSTRING::from(PROP_SIGNAL),
        HSTRING::from(PROP_CONNECTED),
        HSTRING::from(PROP_ADDRESS),
    ];
    let iterable: IIterable<HSTRING> = StringIterable::new(props).into();

    let watcher = DeviceInformation::CreateWatcherWithKindAqsFilterAndAdditionalProperties(
        &aqs_hs,
        &iterable,
        DeviceInformationKind::AssociationEndpoint,
    )?;

    let tx = Arc::new(tx.clone());
    let ctx = Arc::new(ctx.clone());

    // Added
    let tx_a = tx.clone();
    let ctx_a = ctx.clone();
    let added = TypedEventHandler::<
        windows::Devices::Enumeration::DeviceWatcher,
        DeviceInformation,
    >::new(move |_w, info| {
        if let Some(info) = info.as_ref() {
            let id = info.Id().map(|h| h.to_string()).unwrap_or_default();
            let name = info.Name().map(|h| h.to_string()).unwrap_or_default();
            let kind = classify(&id);
            let props = info.Properties().ok();
            let rssi = props
                .as_ref()
                .and_then(|p| lookup_i32(p, PROP_SIGNAL))
                .filter(|&v| is_valid_rssi(v));
            let connected = props
                .as_ref()
                .and_then(|p| lookup_bool(p, PROP_CONNECTED))
                .unwrap_or(false);
            let address = props
                .as_ref()
                .and_then(|p| lookup_string(p, PROP_ADDRESS))
                .unwrap_or_default();
            let _ = tx_a.send(Event::Added {
                id,
                name,
                kind,
                connected,
                rssi,
                address,
            });
            ctx_a.request_repaint();
        }
        Ok(())
    });
    watcher.Added(&added)?;

    // Updated
    let tx_u = tx.clone();
    let ctx_u = ctx.clone();
    let updated = TypedEventHandler::<
        windows::Devices::Enumeration::DeviceWatcher,
        DeviceInformationUpdate,
    >::new(move |_w, upd| {
        if let Some(upd) = upd.as_ref() {
            let id = upd.Id().map(|h| h.to_string()).unwrap_or_default();
            let props = upd.Properties().ok();
            let rssi = props
                .as_ref()
                .and_then(|p| lookup_i32(p, PROP_SIGNAL))
                .filter(|&v| is_valid_rssi(v));
            let connected = props.as_ref().and_then(|p| lookup_bool(p, PROP_CONNECTED));
            let _ = tx_u.send(Event::Updated { id, connected, rssi });
            ctx_u.request_repaint();
        }
        Ok(())
    });
    watcher.Updated(&updated)?;

    // Removed
    let tx_r = tx.clone();
    let ctx_r = ctx.clone();
    let removed = TypedEventHandler::<
        windows::Devices::Enumeration::DeviceWatcher,
        DeviceInformationUpdate,
    >::new(move |_w, upd| {
        if let Some(upd) = upd.as_ref() {
            let id = upd.Id().map(|h| h.to_string()).unwrap_or_default();
            let _ = tx_r.send(Event::Removed(id));
            ctx_r.request_repaint();
        }
        Ok(())
    });
    watcher.Removed(&removed)?;

    // EnumerationCompleted
    // 実測: AEP Watcher では発火しないことがある (rssi_probe poc でも確認済み)。
    // このイベントが来ない前提で、UI 側は「N 秒 Added が来なければ完了扱い」の
    // タイムアウトを見る (ui.rs 側に実装)。ここは念のため登録だけしておく。
    let tx_c = tx.clone();
    let ctx_c = ctx.clone();
    let completed = TypedEventHandler::<
        windows::Devices::Enumeration::DeviceWatcher,
        IInspectable,
    >::new(move |_w, _arg| {
        let _ = tx_c.send(Event::EnumerationComplete);
        ctx_c.request_repaint();
        Ok(())
    });
    watcher.EnumerationCompleted(&completed)?;

    Ok(watcher)
}

// -----------------------------------------------------------------------------
// BLE Advertisement Watcher (未ペアリング BLE を含む近隣機器の検出)
// -----------------------------------------------------------------------------

#[allow(dead_code)] // 参考として残す (現状は Service Data 側で判別)
const SWITCHBOT_COMPANY_ID: u16 = 0x0969;

/// SwitchBot Service UUID (バイト単位で LE 順の照合ではなく u16 比較で使う)
const SWITCHBOT_UUID_OLDGEN: u16 = 0xFD3D;
const SWITCHBOT_UUID_NEWGEN: u16 = 0xFCD2;

/// AD Type: Service Data - 16-bit UUID
const AD_TYPE_SERVICE_DATA_16: u8 = 0x16;

/// 新世代 SwitchBot の 4 バイト model ID (poc/switchbot_probe と同じ)
/// 実測: ServiceData(0xFD3D) の bytes[3..7] に埋まっている
const NEWGEN_MODEL_IDS: &[(&[u8; 4], &str)] = &[
    (&[0x00, 0x10, 0xB9, 0x40], "SwitchBot Hub 3"),
    (&[0x01, 0x10, 0xB9, 0x40], "SwitchBot Hub 3"),
    (&[0x00, 0x10, 0xA5, 0xB8], "SwitchBot Lock Ultra"),
    (&[0x01, 0x10, 0xA5, 0xB8], "SwitchBot Lock Ultra"),
    (&[0x00, 0x11, 0x03, 0x78], "SwitchBot Keypad Vision"),
    (&[0x01, 0x11, 0x03, 0x78], "SwitchBot Keypad Vision"),
    (&[0x00, 0x11, 0x51, 0x98], "SwitchBot Keypad Vision Pro"),
    (&[0x01, 0x11, 0x51, 0x98], "SwitchBot Keypad Vision Pro"),
];

fn build_ble_advertisement_watcher(
    tx: &Sender<Event>,
    ctx: &egui::Context,
) -> Result<windows::Devices::Bluetooth::Advertisement::BluetoothLEAdvertisementWatcher> {
    use windows::Devices::Bluetooth::Advertisement::{
        BluetoothLEAdvertisementReceivedEventArgs, BluetoothLEAdvertisementWatcher,
        BluetoothLEScanningMode,
    };
    use windows::Devices::Bluetooth::BluetoothSignalStrengthFilter;

    let watcher = BluetoothLEAdvertisementWatcher::new()?;
    watcher.SetScanningMode(BluetoothLEScanningMode::Active)?;

    // SamplingInterval=500ms で per-device スロットル。
    // これが無いと 35 pkt/s x 9 台 = 300+ ev/s になって UI 側が詰まる。
    // poc/ble_ad_filter で挙動確認済み。
    let filter = BluetoothSignalStrengthFilter::new()?;
    filter.SetSamplingInterval(&box_timespan_ms(500)?)?;
    watcher.SetSignalStrengthFilter(&filter)?;

    let tx = Arc::new(tx.clone());
    let ctx = Arc::new(ctx.clone());

    watcher.Received(&TypedEventHandler::<
        BluetoothLEAdvertisementWatcher,
        BluetoothLEAdvertisementReceivedEventArgs,
    >::new(move |_w, args| {
        if let Some(args) = args.as_ref() {
            let address = args.BluetoothAddress().unwrap_or(0);
            let rssi = args.RawSignalStrengthInDBm().unwrap_or(0);
            // -127 dBm は SignalStrengthFilter の OutOfRange センチネル
            if rssi == -127 { return Ok(()); }

            let mut name = None;
            let mut mfr_company = None;
            let mut model_hint = None;

            if let Ok(ad) = args.Advertisement() {
                if let Ok(ln) = ad.LocalName() {
                    let s = ln.to_string();
                    if !s.is_empty() { name = Some(s); }
                }

                // Manufacturer Data の最初の Company ID を取る
                if let Ok(mfrs) = ad.ManufacturerData() {
                    if let Ok(size) = mfrs.Size() {
                        if size > 0 {
                            if let Ok(m) = mfrs.GetAt(0) {
                                mfr_company = m.CompanyId().ok();
                            }
                        }
                    }
                }

                // SwitchBot 判別 (旧世代: ServiceData[0]&0x7F ASCII, 新世代: [3..7] 4-byte ID)
                if let Ok(sections) = ad.DataSections() {
                    for i in 0..sections.Size().unwrap_or(0) {
                        let Ok(sec) = sections.GetAt(i) else { continue };
                        let dtype = sec.DataType().unwrap_or(0);
                        if dtype != AD_TYPE_SERVICE_DATA_16 { continue; }
                        let Ok(buf) = sec.Data() else { continue };
                        let bytes = read_buffer(&buf);
                        if bytes.len() < 2 { continue; }
                        let uuid = u16::from_le_bytes([bytes[0], bytes[1]]);
                        if uuid != SWITCHBOT_UUID_OLDGEN && uuid != SWITCHBOT_UUID_NEWGEN {
                            continue;
                        }
                        let payload = &bytes[2..];
                        if payload.is_empty() { continue; }

                        // 旧世代 ASCII 判定
                        let b0 = payload[0];
                        let masked = b0 & 0x7F;
                        if let Some(n) = switchbot_ascii(masked) {
                            model_hint = Some(n.into());
                            break;
                        }

                        // 新世代 4 バイト ID 判定
                        if payload.len() >= 7 {
                            let mid: [u8; 4] = [payload[3], payload[4], payload[5], payload[6]];
                            for (want, name) in NEWGEN_MODEL_IDS {
                                if &mid == *want {
                                    model_hint = Some((*name).into());
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            let _ = tx.send(Event::NearbyBleSeen {
                address, rssi, name, mfr_company, model_hint,
            });
            ctx.request_repaint();
        }
        Ok(())
    }))?;

    watcher.Start()?;
    Ok(watcher)
}

/// pySwitchbot v2 SUPPORTED_TYPES の ASCII 部分
fn switchbot_ascii(masked: u8) -> Option<&'static str> {
    match masked {
        b'H' => Some("SwitchBot Bot"),
        b'T' => Some("SwitchBot Meter"),
        b'i' => Some("SwitchBot Meter Plus"),
        b'w' | b'W' => Some("SwitchBot Meter Pro"),
        b'c' | b'C' => Some("SwitchBot Curtain"),
        b'{' => Some("SwitchBot Curtain 3"),
        b'd' | b'D' => Some("SwitchBot Contact"),
        b's' | b'S' => Some("SwitchBot Motion"),
        b'r' | b'R' => Some("SwitchBot Light Strip"),
        b'j' | b'J' => Some("SwitchBot Plug Mini (JP)"),
        b'g' | b'G' => Some("SwitchBot Plug Mini"),
        b'u' | b'U' => Some("SwitchBot Color Bulb"),
        b'e' | b'E' => Some("SwitchBot Humidifier"),
        b'y' | b'Y' => Some("SwitchBot Keypad"),
        b'v' | b'V' => Some("SwitchBot Hub 2"),
        b'o' | b'O' => Some("SwitchBot Lock"),
        b'l' | b'L' => Some("SwitchBot Blind Tilt"),
        _ => None,
    }
}

fn box_timespan_ms(ms: i64) -> windows::core::Result<windows::Foundation::IReference<windows::Foundation::TimeSpan>> {
    let ts = windows::Foundation::TimeSpan { Duration: ms * 10_000 }; // 100ns 単位
    let pv = windows::Foundation::PropertyValue::CreateTimeSpan(ts)?;
    pv.cast()
}

fn read_buffer(buf: &windows::Storage::Streams::IBuffer) -> Vec<u8> {
    use windows::Storage::Streams::DataReader;
    let reader = match DataReader::FromBuffer(buf) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let len = reader.UnconsumedBufferLength().unwrap_or(0) as usize;
    let mut out = vec![0u8; len];
    let _ = reader.ReadBytes(&mut out);
    out
}

// -----------------------------------------------------------------------------
// Bluetooth Classic Inquiry (Win32 API)
// poc/classic_inquiry と同ロジック。BluetoothFindFirstDevice で新規 Inquiry を
// 発行し、found を Event::ClassicFound で流す。約 8 秒ブロック。
// -----------------------------------------------------------------------------
fn run_classic_inquiry(tx: &Sender<Event>, ctx: &egui::Context) -> u32 {
    use windows::Win32::Devices::Bluetooth::{
        BluetoothFindDeviceClose, BluetoothFindFirstDevice, BluetoothFindNextDevice,
        BLUETOOTH_DEVICE_INFO, BLUETOOTH_DEVICE_SEARCH_PARAMS,
    };
    use windows::Win32::Foundation::HANDLE;

    let mut params = BLUETOOTH_DEVICE_SEARCH_PARAMS {
        dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_SEARCH_PARAMS>() as u32,
        fReturnAuthenticated: true.into(),
        fReturnRemembered: true.into(),
        fReturnUnknown: true.into(),
        fReturnConnected: true.into(),
        fIssueInquiry: true.into(),
        cTimeoutMultiplier: 6, // 6 × 1.28s ≈ 7.68 秒
        hRadio: HANDLE::default(),
    };

    let mut info = BLUETOOTH_DEVICE_INFO {
        dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_INFO>() as u32,
        ..Default::default()
    };

    let hfind = unsafe { BluetoothFindFirstDevice(&mut params, &mut info) };
    let Ok(hfind) = hfind else { return 0 };

    let mut count = 0u32;
    loop {
        count += 1;
        emit_classic(tx, ctx, &info);
        unsafe {
            info = BLUETOOTH_DEVICE_INFO {
                dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_INFO>() as u32,
                ..Default::default()
            };
            if BluetoothFindNextDevice(hfind, &mut info).is_err() {
                break;
            }
        }
    }
    unsafe { let _ = BluetoothFindDeviceClose(hfind); }
    count
}

fn emit_classic(
    tx: &Sender<Event>,
    ctx: &egui::Context,
    d: &windows::Win32::Devices::Bluetooth::BLUETOOTH_DEVICE_INFO,
) {
    let addr_bytes = unsafe { d.Address.Anonymous.rgBytes };
    // rgBytes は LE (低位バイトから)。u64 の 48-bit MAC に変換
    let address: u64 = (addr_bytes[5] as u64) << 40
        | (addr_bytes[4] as u64) << 32
        | (addr_bytes[3] as u64) << 24
        | (addr_bytes[2] as u64) << 16
        | (addr_bytes[1] as u64) << 8
        | (addr_bytes[0] as u64);

    let name_bytes: Vec<u16> = d.szName.iter().take_while(|&&c| c != 0).copied().collect();
    let name = String::from_utf16_lossy(&name_bytes);

    let _ = tx.send(Event::ClassicFound {
        address,
        name,
        class_of_device: d.ulClassofDevice,
        connected: d.fConnected.as_bool(),
        paired: d.fAuthenticated.as_bool(),
        remembered: d.fRemembered.as_bool(),
    });
    ctx.request_repaint();
}

fn extract_bt_radio_state(radios: &windows_collections::IVectorView<Radio>) -> RadioState {
    let n = radios.Size().unwrap_or(0);
    for i in 0..n {
        if let Ok(r) = radios.GetAt(i) {
            if r.Kind().ok() == Some(RadioKind::Bluetooth) {
                return match r.State().unwrap_or(WinRadioState::Unknown) {
                    WinRadioState::On => RadioState::On,
                    WinRadioState::Off => RadioState::Off,
                    WinRadioState::Disabled => RadioState::Disabled,
                    _ => RadioState::Unknown,
                };
            }
        }
    }
    RadioState::Absent
}

fn classify(id: &str) -> DeviceKind {
    if id.starts_with("BluetoothLE") {
        DeviceKind::Ble
    } else {
        DeviceKind::Classic
    }
}

fn is_valid_rssi(v: i32) -> bool {
    // 0 dBm は WinRT の「未測定」センチネル。物理的にあり得ない値。
    // -127 dBm は SignalStrengthFilter の OutOfRange マーカー (AEP ルートでは
    // 発生しないはずだが安全側で除外)。
    // 有効範囲: -126 dBm 以上 -1 dBm 以下
    v < 0 && v > -127
}

fn lookup_i32(props: &IMapView<HSTRING, IInspectable>, key: &str) -> Option<i32> {
    let k = HSTRING::from(key);
    if !props.HasKey(&k).ok()? {
        return None;
    }
    let v = props.Lookup(&k).ok()?;
    let pv: IPropertyValue = v.cast().ok()?;
    pv.GetInt32().ok()
}

fn lookup_bool(props: &IMapView<HSTRING, IInspectable>, key: &str) -> Option<bool> {
    let k = HSTRING::from(key);
    if !props.HasKey(&k).ok()? {
        return None;
    }
    let v = props.Lookup(&k).ok()?;
    let pv: IPropertyValue = v.cast().ok()?;
    pv.GetBoolean().ok()
}

fn lookup_string(props: &IMapView<HSTRING, IInspectable>, key: &str) -> Option<String> {
    let k = HSTRING::from(key);
    if !props.HasKey(&k).ok()? {
        return None;
    }
    let v = props.Lookup(&k).ok()?;
    let pv: IPropertyValue = v.cast().ok()?;
    pv.GetString().ok().map(|h| h.to_string())
}

// ---- IIterable<HSTRING> の自前実装 (poc と同じ) ---------------------------

#[implement(IIterable<HSTRING>)]
struct StringIterable {
    items: Vec<HSTRING>,
}
impl StringIterable {
    fn new(items: Vec<HSTRING>) -> Self { Self { items } }
}
impl IIterable_Impl<HSTRING> for StringIterable_Impl {
    fn First(&self) -> windows::core::Result<IIterator<HSTRING>> {
        Ok(StringIterator {
            items: self.items.clone(),
            index: Mutex::new(0),
        }.into())
    }
}
#[implement(IIterator<HSTRING>)]
struct StringIterator {
    items: Vec<HSTRING>,
    index: Mutex<usize>,
}
impl IIterator_Impl<HSTRING> for StringIterator_Impl {
    fn Current(&self) -> windows::core::Result<HSTRING> {
        let i = *self.index.lock().unwrap();
        self.items
            .get(i)
            .cloned()
            .ok_or_else(|| windows::core::Error::from_hresult(windows::core::HRESULT(0x8000000B_u32 as i32)))
    }
    fn HasCurrent(&self) -> windows::core::Result<bool> {
        Ok(*self.index.lock().unwrap() < self.items.len())
    }
    fn MoveNext(&self) -> windows::core::Result<bool> {
        let mut g = self.index.lock().unwrap();
        *g = g.saturating_add(1);
        Ok(*g < self.items.len())
    }
    fn GetMany(&self, items: &mut [HSTRING]) -> windows::core::Result<u32> {
        let mut g = self.index.lock().unwrap();
        let avail = self.items.len().saturating_sub(*g);
        let n = items.len().min(avail);
        for k in 0..n { items[k] = self.items[*g + k].clone(); }
        *g += n;
        Ok(n as u32)
    }
}
