# SimpleBluetooth

Windows PC 用の Bluetooth 状態確認ツール。ペアリング済み機器・近くの BLE 広告・
Classic Inquiry の 3 ビューで、周辺 Bluetooth 環境を可視化します。

## スクリーンショットで見る位置づけ

「電波環境の可視化」に特化した軽量ビューア。ペアリング・接続・切断のような
操作は含みません (Windows 標準の Bluetooth 設定でどうぞ)。

## 機能

### ペアリング済みタブ
- ペアリング済みの BLE 機器を一覧表示 (名前 / RSSI / 信号バー / 接続状態)
- RSSI 値には「N 秒前」を併記 (古い値はバーを半透明化)
- Classic 機器の表示は既定 OFF (「設定」から手動有効化。理由は下記の注意事項)

### 近くの BLE タブ
- `BluetoothLEAdvertisementWatcher` で BLE アドバタイズを直接受信
- 500ms スロットルで受信頻度を制御
- SwitchBot 系機器 (Bot / Meter / Curtain / Hub / Lock 等) を自動識別
- Manufacturer Data の Company ID を表示

### Classic 探索タブ
- 手動ボタンで Bluetooth Classic Inquiry を発行 (約 8 秒ブロッキング)
- ペアリングモードの機器を発見する用途 (RSSI は取得不可)
- Class of Device の Major を分類表示 (Audio/Video, Peripheral 等)

### 共通
- Bluetooth ラジオ状態インジケータ (ON / OFF / 無効 / 無し)
- 3 択テーマ (OS 準拠 / ダーク / ライト) — タイトルバーもテーマ追従
- 「更新」ボタンで Watcher を再作成 (新規ペアリング分の反映用)
- 設定はモーダルダイアログで隔離

## 電波強度 (RSSI) の注意事項

**表示値は Windows が記録した最終値で、リアルタイム測定ではありません。**

- 更新間隔は Windows 任せ (数秒〜数分)
- 「N 秒前」表記は Windows からイベントを受け取った時刻であり、実測時刻とは異なる可能性
- ペアリング済み機器は Windows AEP プロパティ経由 (`System.Devices.Aep.SignalStrength`)、
  近くの BLE タブは広告パケット直受け (絶対 dBm)
- **Classic 機器の RSSI は機種・ドライバによっては絶対 dBm ではなく
  Bluetooth 規格の Golden Range 相対値がそのまま露出する場合があります**。
  機器間の絶対比較には使えないので、既定で非表示にしています

相対的な変化を眺める用途を推奨します。

## 動作条件

- Windows 10 / Windows 11
- 内蔵または USB Bluetooth アダプタ

位置情報サービスの許可は不要 (BLE アドバタイズ受信は Passive/Active どちらも許可なしで動作)。

## ビルド

Rust ツールチェイン (安定版) が必要です。

```bash
cd app
cargo build --release
```

成果物: `app/target/release/simplebluetooth.exe` (単一実行ファイル)

インストーラーやレジストリ書き込みはありません。設定ファイル (テーマ選択のみ)
は `%APPDATA%\SimpleBluetooth\config.toml` に保存されます。

## 技術構成

- Rust + `eframe` / `egui` 0.29
- `windows` クレート (Windows Runtime API 経由で WinRT Bluetooth を叩く)
- ペアリング済み機器: `Windows.Devices.Enumeration.DeviceWatcher`
- BLE 広告: `Windows.Devices.Bluetooth.Advertisement.BluetoothLEAdvertisementWatcher`
- Classic Inquiry: Win32 `BluetoothFindFirstDevice` / `BluetoothFindNextDevice`

## ライセンス

MIT License (`LICENSE` を参照)
