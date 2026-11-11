//! Runtime brand icons are bounded RGBA pixels, never arbitrary filesystem paths.
use std::{fs, path::PathBuf, sync::Mutex};
use tauri::Manager;

const SIDE: u32 = 256;
const BYTES: usize = 256 * 256 * 4;
static UPDATE: Mutex<()> = Mutex::new(());
fn folder() -> PathBuf { crate::registry::base_dir().join("brand") }

pub fn icon_path() -> Option<PathBuf> {
    let name = fs::read_to_string(folder().join("active.txt")).ok()?;
    if !name.starts_with("icon-") || !name.ends_with(".ico") || name.contains(['/', '\\']) { return None; }
    let path = folder().join(name);
    path.is_file().then_some(path)
}

fn resized(rgba: &[u8], side: u32) -> Vec<u8> {
    let original = image::RgbaImage::from_raw(SIDE, SIDE, rgba.to_vec()).expect("validated RGBA");
    image::imageops::resize(&original, side, side, image::imageops::FilterType::Lanczos3).into_raw()
}

// Each DIB has its own correctly padded AND mask, including 20/24px DPI sizes.
fn dib(rgba: &[u8], side: u32) -> Vec<u8> {
    let bytes = side * side * 4;
    let mut out = Vec::new();
    out.extend(40u32.to_le_bytes()); out.extend(side.to_le_bytes()); out.extend((side * 2).to_le_bytes());
    out.extend(1u16.to_le_bytes()); out.extend(32u16.to_le_bytes());
    out.extend(0u32.to_le_bytes()); out.extend(bytes.to_le_bytes()); out.extend([0;16]);
    let side = side as usize;
    for y in (0..side).rev() { for x in 0..side {
        let i = (y * side + x) * 4;
        out.extend([rgba[i+2], rgba[i+1], rgba[i], rgba[i+3]]);
    }}
    for y in (0..side).rev() { for byte in 0..side.div_ceil(32)*4 {
        let mut mask = 0u8;
        for bit in 0..8 { let x = byte*8+bit; if x < side && rgba[(y*side+x)*4+3] == 0 { mask |= 0x80 >> bit; } }
        out.push(mask);
    }}
    out
}

fn ico(rgba: &[u8]) -> Vec<u8> {
    let sizes = [16u32,20,24,32,40,48,64,96,128,256];
    let frames: Vec<_> = sizes.iter().map(|&s| dib(&resized(rgba,s),s)).collect();
    let mut out = vec![0,0,1,0]; out.extend((sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + sizes.len() as u32 * 16;
    for (&side,frame) in sizes.iter().zip(&frames) {
        out.extend([side as u8,side as u8,0,0,1,0,32,0]);
        out.extend((frame.len() as u32).to_le_bytes());out.extend(offset.to_le_bytes());offset += frame.len() as u32;
    }
    for frame in frames {out.extend(frame)}
    out
}

fn apply(app: &tauri::AppHandle, rgba: Vec<u8>) -> Result<(), String> {
    #[cfg(windows)]
    if let (Some(window),Some(path)) = (app.get_webview_window("main"),icon_path()) { native::window_icons(&window,&path)?; }
    #[cfg(not(windows))]
    if let Some(window) = app.get_webview_window("main") { window.set_icon(tauri::image::Image::new_owned(rgba.clone(),SIDE,SIDE)).map_err(|e|e.to_string())?; }
    #[cfg(windows)] let tray_side = native::small_icon_size();
    #[cfg(not(windows))] let tray_side = 32;
    let icon = tauri::image::Image::new_owned(resized(&rgba,tray_side),tray_side,tray_side);
    if let Some(tray) = app.tray_by_id(crate::TRAY_ID) { tray.set_icon(Some(icon)).map_err(|e| e.to_string())?; }
    Ok(())
}

pub fn restore(app: &tauri::AppHandle) {
    if let Ok(bytes) = fs::read(folder().join("active.rgba")) {
        if bytes.len() == BYTES { let _ = apply(app, bytes); }
    }
}

#[tauri::command]
pub async fn set_brand_icon(app: tauri::AppHandle, rgba: Vec<u8>) -> Result<(), String> {
    if rgba.len() != BYTES { return Err("图标必须为 256 × 256 RGBA".into()); }
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = UPDATE.lock().map_err(|e| e.to_string())?;
        let dir = folder(); fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let mut hash = 0xcbf29ce484222325u64;
        for byte in &rgba { hash = (hash ^ *byte as u64).wrapping_mul(0x100000001b3); }
        let name = format!("icon-v2-{hash:016x}.ico");
        fs::write(dir.join(&name), ico(&rgba)).map_err(|e| e.to_string())?;
        fs::write(dir.join("active.rgba"), &rgba).map_err(|e| e.to_string())?;
        fs::write(dir.join("active.txt"), name).map_err(|e| e.to_string())?;
        apply(&app, rgba)?;
        // 每次都同步，不再看"内容是否变了"。外壳（桌面/开始菜单/任务栏固定项）读的是各自
        // 快捷方式里的 IconLocation，运行中的窗口图标顶不了它们；只要有一条没指到当前 ico，
        // 界面上就是"设置里换了、桌面没换"。同步本身幂等：已指对的不重写。
        crate::shelllink::sync_brand_icons()
            .map_err(|e| format!("图标已应用，但外壳快捷方式同步失败：{e}"))?;
        Ok(())
    }).await.map_err(|e| e.to_string())?
}

#[cfg(windows)]
mod native {
    use std::{ffi::c_void,path::Path,sync::Mutex};
    // Keep our own handles alive while Windows uses them; never destroy Tauri's.
    static ICONS: Mutex<(isize,isize)> = Mutex::new((0,0));
    #[link(name="user32")]
    extern "system" {
        fn LoadImageW(instance: *mut c_void, name: *const u16, kind: u32, cx: i32, cy: i32, flags: u32) -> *mut c_void;
        fn SendMessageW(hwnd: *mut c_void, msg: u32, wp: usize, lp: isize) -> isize;
        fn DestroyIcon(icon: *mut c_void) -> i32;
        fn GetSystemMetricsForDpi(index: i32, dpi: u32) -> i32;
        fn GetDpiForSystem() -> u32;
        fn GetDpiForWindow(hwnd: *mut c_void) -> u32;
    }
    pub fn small_icon_size() -> u32 { unsafe { GetSystemMetricsForDpi(49,GetDpiForSystem()).clamp(16,64) as u32 } }
    pub fn window_icons(window: &tauri::WebviewWindow, path: &Path) -> Result<(),String> {
        let hwnd = window.hwnd().map_err(|e|e.to_string())?.0;
        let wide: Vec<u16> = path.as_os_str().to_string_lossy().encode_utf16().chain(Some(0)).collect();
        let mut icons=ICONS.lock().map_err(|e|e.to_string())?;
        unsafe {
            let dpi=GetDpiForWindow(hwnd).max(96);
            let small=GetSystemMetricsForDpi(49,dpi); let big=GetSystemMetricsForDpi(11,dpi);
            let a=LoadImageW(std::ptr::null_mut(),wide.as_ptr(),1,small,small,0x10);
            let b=LoadImageW(std::ptr::null_mut(),wide.as_ptr(),1,big,big,0x10);
            if a.is_null() || b.is_null() {
                if !a.is_null(){DestroyIcon(a);} if !b.is_null(){DestroyIcon(b);}
                return Err("无法加载多尺寸系统图标".into());
            }
            SendMessageW(hwnd,0x80,0,a as isize); SendMessageW(hwnd,0x80,1,b as isize);
            for old in [icons.0,icons.1] {if old!=0 {DestroyIcon(old as *mut c_void);}}
            *icons=(a as isize,b as isize);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ico_layout_and_alpha_mask() {
        let mut rgba = vec![0; super::BYTES];
        let i = 255*256*4; rgba[i..i+4].copy_from_slice(&[10,20,30,255]);
        let bytes = super::dib(&rgba,256);
        assert_eq!(bytes.len(), 40+super::BYTES+8192);
        assert_eq!(&bytes[40..44], &[30,20,10,255]);
        assert_eq!(bytes[40+super::BYTES], 0x7f);
        let packed=super::ico(&rgba);
        assert_eq!(&packed[..6], &[0,0,1,0,10,0]);
        for i in 0..10 {
            let header=6+i*16;
            let side=if packed[header]==0 {256} else {packed[header] as usize};
            let len=u32::from_le_bytes(packed[header+8..header+12].try_into().unwrap()) as usize;
            assert_eq!(len,40+side*side*4+side.div_ceil(32)*4*side);
        }
    }
}
