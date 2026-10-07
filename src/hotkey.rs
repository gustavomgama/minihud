use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS,
};

pub const HOTKEY_ID_TOGGLE: i32 = 1;
pub const HOTKEY_ID_CLICKTHROUGH: i32 = 2;

pub fn register(hwnd: HWND) -> windows::core::Result<()> {
    unsafe {
        RegisterHotKey(Some(hwnd), HOTKEY_ID_TOGGLE, HOT_KEY_MODIFIERS(0), 0x76)?; // VK_F7
        RegisterHotKey(
            Some(hwnd),
            HOTKEY_ID_CLICKTHROUGH,
            HOT_KEY_MODIFIERS(0),
            0x77,
        )?; // VK_F8
    }
    Ok(())
}

pub fn unregister(hwnd: HWND) {
    unsafe {
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID_TOGGLE);
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID_CLICKTHROUGH);
    }
}
