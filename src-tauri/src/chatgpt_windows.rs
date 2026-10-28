//! Match real Electron document windows, including ones hidden by Close-to-tray.
//! A titled notification helper is not a client window. Never use the process'
//! MainWindowHandle: Windows often chooses that helper after the real window hides.

#[derive(Debug)]
struct Window {
    handle: isize,
    pid: u32,
    visible: bool,
    owner: isize,
    style: u32,
    ex_style: u32,
    class: String,
    title: String,
}

fn document_window(w: &Window, pids: &[u32]) -> bool {
    pids.contains(&w.pid)
        && w.class == "Chrome_WidgetWin_1"
        && w.owner == 0
        && w.style & 0x00c0_0000 == 0x00c0_0000 // WS_CAPTION
        && w.ex_style & 0x0000_0080 == 0 // WS_EX_TOOLWINDOW
        && !w.title.trim().is_empty()
        && !w.title.to_ascii_lowercase().starts_with("devtools")
}

#[cfg(windows)]
mod native {
    use super::Window;
    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(cb: unsafe extern "system" fn(isize, isize) -> i32, param: isize) -> i32;
        fn IsWindowVisible(hwnd: isize) -> i32;
        fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
        fn GetWindowTextW(hwnd: isize, text: *mut u16, count: i32) -> i32;
        fn GetClassNameW(hwnd: isize, text: *mut u16, count: i32) -> i32;
        fn GetWindow(hwnd: isize, command: u32) -> isize;
        fn GetWindowLongW(hwnd: isize, index: i32) -> i32;
        fn IsIconic(hwnd: isize) -> i32;
        fn ShowWindowAsync(hwnd: isize, command: i32) -> i32;
        fn BringWindowToTop(hwnd: isize) -> i32;
        fn SetForegroundWindow(hwnd: isize) -> i32;
        #[cfg(test)]
        fn PostMessageW(hwnd: isize, message: u32, wparam: usize, lparam: isize) -> i32;
    }
    struct Collection<'a> { pids: &'a [u32], windows: Vec<Window> }
    unsafe extern "system" fn collect(hwnd: isize, param: isize) -> i32 {
        let set = &mut *(param as *mut Collection<'_>);
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if !set.pids.contains(&pid) { return 1; }
        let mut class = [0u16; 128];
        let class_len = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32).max(0) as usize;
        let mut title = [0u16; 1024];
        let title_len = GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32).max(0) as usize;
        set.windows.push(Window {
            handle: hwnd, pid, visible: IsWindowVisible(hwnd) != 0,
            owner: GetWindow(hwnd, 4), // GW_OWNER
            style: GetWindowLongW(hwnd, -16) as u32,
            ex_style: GetWindowLongW(hwnd, -20) as u32,
            class: String::from_utf16_lossy(&class[..class_len]),
            title: String::from_utf16_lossy(&title[..title_len]),
        });
        1
    }
    pub fn windows(pids: &[u32]) -> Vec<Window> {
        if pids.is_empty() { return Vec::new(); }
        let mut set = Collection { pids, windows: Vec::new() };
        unsafe { EnumWindows(collect, &mut set as *mut Collection<'_> as isize); }
        set.windows
    }
    pub fn show(w: &Window, pids: &[u32]) -> bool {
        // Recheck ownership before acting: HWNDs can be recycled after Close.
        unsafe {
            let mut pid = 0;
            GetWindowThreadProcessId(w.handle, &mut pid);
            if pid != w.pid || !pids.contains(&pid) { return false; }
            ShowWindowAsync(w.handle, if IsIconic(w.handle) != 0 { 9 } else { 5 });
            BringWindowToTop(w.handle);
            SetForegroundWindow(w.handle);
        }
        // ShowWindowAsync only queues work on the owner thread. Confirm that
        // the same owned window actually became visible before reporting success.
        let until = std::time::Instant::now() + std::time::Duration::from_millis(700);
        loop {
            unsafe {
                let mut pid = 0;
                GetWindowThreadProcessId(w.handle, &mut pid);
                if pid != w.pid || !pids.contains(&pid) { return false; }
                if IsWindowVisible(w.handle) != 0 { return true; }
            }
            if std::time::Instant::now() >= until { return false; }
            std::thread::sleep(std::time::Duration::from_millis(35));
        }
    }
    #[cfg(test)]
    pub fn close(w: &Window, pids: &[u32]) -> bool {
        unsafe {
            let mut pid = 0;
            GetWindowThreadProcessId(w.handle, &mut pid);
            pid == w.pid && pids.contains(&pid) && PostMessageW(w.handle, 0x0010, 0, 0) != 0
        }
    }
}

#[cfg(not(windows))]
mod native {
    use super::Window;
    pub fn windows(_: &[u32]) -> Vec<Window> { Vec::new() }
    pub fn show(_: &Window, _: &[u32]) -> bool { false }
}

pub fn visible(pids: &[u32]) -> bool {
    native::windows(pids).iter().any(|w| w.visible && document_window(w, pids))
}

/// Show this account's existing real window. Return false only when it is gone;
/// the caller can then ask Electron's profile-scoped singleton to create it again.
pub fn restore(pids: &[u32]) -> bool {
    let mut windows = native::windows(pids);
    // Prefer the visible document if an app temporarily has multiple windows.
    windows.sort_by_key(|w| !w.visible);
    windows.iter().filter(|w| document_window(w, pids)).any(|w| native::show(w, pids))
}

/// Only compiled into the explicitly opted-in real-window smoke test. Close
/// exactly one real window whose account was fully stopped before the test.
#[cfg(all(test, windows))]
pub fn close_for_smoke(pids: &[u32]) -> bool {
    native::windows(pids).iter().filter(|w| w.visible && document_window(w, pids))
        .any(|w| native::close(w, pids))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn example() -> Window {
        Window { handle: 1, pid: 42, visible: false, owner: 0, style: 0x04c7_0000,
            ex_style: 0x0024_0100, class: "Chrome_WidgetWin_1".into(), title: "ChatGPT".into() }
    }
    #[test]
    fn hidden_real_window_is_restorable_but_notification_and_ipc_windows_are_not() {
        let mut w = example();
        assert!(document_window(&w, &[42]));
        assert!(!document_window(&w, &[43]));
        w.ex_style = 0x0020_0088;
        assert!(!document_window(&w, &[42]));
        w = example(); w.style = 0x0400_0000;
        assert!(!document_window(&w, &[42]));
        w = example(); w.owner = 100;
        assert!(!document_window(&w, &[42]));
        w = example(); w.title.clear();
        assert!(!document_window(&w, &[42]));
        w = example(); w.title = "DevTools - ChatGPT".into();
        assert!(!document_window(&w, &[42]));
    }
}
