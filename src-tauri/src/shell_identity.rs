//! Keep our windows and shortcuts out of a parent's Windows Store taskbar group.
pub const APP_ID: &str = "com.renlu.agent-cockpit";

#[cfg(windows)]
mod platform {
    use super::APP_ID;
    use windows::core::{GUID, PCWSTR};
    use windows::Win32::Foundation::{HWND, PROPERTYKEY};
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
    use windows::Win32::System::Variant::VT_LPWSTR;
    use windows::Win32::UI::Shell::{SetCurrentProcessExplicitAppUserModelID, SHStrDupW};
    use windows::Win32::UI::Shell::PropertiesSystem::{
        IPropertyStore, SHGetPropertyStoreForWindow, SHGetPropertyStoreFromParsingName, GPS_READWRITE,
    };

    const APP_ID_KEY: PROPERTYKEY = PROPERTYKEY {
        fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3), pid: 5,
    };

    fn wide(text: &str) -> Vec<u16> { text.encode_utf16().chain(Some(0)).collect() }

    fn value() -> windows::core::Result<PROPVARIANT> {
        let text = wide(APP_ID);
        // Property stores require a VT_LPWSTR allocated by COM, not a BSTR.
        let owned = unsafe { SHStrDupW(PCWSTR(text.as_ptr()))? };
        Ok(PROPVARIANT { Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_LPWSTR, wReserved1: 0, wReserved2: 0, wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 { pwszVal: owned },
            }),
        } })
    }

    pub fn process() -> Result<(), String> {
        let text = wide(APP_ID);
        unsafe { SetCurrentProcessExplicitAppUserModelID(PCWSTR(text.as_ptr())) }
            .map_err(|e| format!("设置驾驶舱进程标识失败：{e}"))
    }

    pub fn window(window: &tauri::WebviewWindow) -> Result<(), String> {
        let handle = window.hwnd().map_err(|e| e.to_string())?;
        let store: IPropertyStore = unsafe { SHGetPropertyStoreForWindow(HWND(handle.0)) }
            .map_err(|e| e.to_string())?;
        let value = value().map_err(|e| e.to_string())?;
        // Window property stores notify the taskbar immediately; no Commit needed.
        unsafe { store.SetValue(&APP_ID_KEY, &value) }.map_err(|e| e.to_string())
    }

    pub fn shortcut(path: &std::path::Path) -> Result<(), String> {
        // Background shortcut maintenance has its own COM apartment. An existing
        // apartment is also usable; only balance successful initialization calls.
        let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let result = (|| -> windows::core::Result<()> {
            let text = wide(&path.to_string_lossy());
            let store: IPropertyStore = unsafe {
                SHGetPropertyStoreFromParsingName(PCWSTR(text.as_ptr()), None, GPS_READWRITE)?
            };
            let value = value()?;
            unsafe { store.SetValue(&APP_ID_KEY, &value)?; store.Commit() }
        })();
        if initialized.is_ok() { unsafe { CoUninitialize(); } }
        result.map_err(|e| format!("设置快捷方式标识失败：{e}"))
    }
}

pub fn process() -> Result<(), String> {
    #[cfg(windows)] { return platform::process(); }
    #[cfg(not(windows))] { Ok(()) }
}
pub fn window(window: &tauri::WebviewWindow) -> Result<(), String> {
    #[cfg(windows)] { return platform::window(window); }
    #[cfg(not(windows))] { let _ = window; Ok(()) }
}
pub fn shortcut(path: &std::path::Path) -> Result<(), String> {
    #[cfg(windows)] { return platform::shortcut(path); }
    #[cfg(not(windows))] { let _ = path; Ok(()) }
}
