// Windows タイトルバーのダークモード追従
//
// egui/eframe はクライアント領域のテーマしか制御しない。Windows のタイトルバーは
// 非クライアント領域なので DwmSetWindowAttribute(DWMWA_USE_IMMERSIVE_DARK_MODE) で
// 明示的にダーク化する必要がある ([[win32-cpp-darkmode-pitfalls]] 参照)。

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE,
};

pub fn apply(frame: &eframe::Frame, dark: bool) {
    let handle = match frame.window_handle() {
        Ok(h) => h,
        Err(_) => return,
    };
    let hwnd = match handle.as_raw() {
        RawWindowHandle::Win32(h) => HWND(h.hwnd.get() as *mut _),
        _ => return,
    };
    let value: i32 = if dark { 1 } else { 0 };
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &value as *const _ as *const _,
            std::mem::size_of::<i32>() as u32,
        );
    }
}
