//! Native notification-area icon. All callbacks run on the GUI thread.
use anyhow::{Context, Result};
use eframe::egui;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

const CALLBACK: u32 = WM_APP + 73;
const SUBCLASS: usize = 0x41535452;
const OPEN: usize = 1;
const EXIT: usize = 2;

struct Data {
    icon: NOTIFYICONDATAW,
    taskbar_created: u32,
    exit: Arc<AtomicBool>,
    ctx: egui::Context,
}

pub struct Tray {
    data: Box<Data>,
}

impl Tray {
    pub fn new(hwnd: HWND, ctx: egui::Context) -> Result<Self> {
        unsafe {
            // Reuse eframe's application icon; ownership stays with the window.
            let icon = SendMessageW(
                hwnd,
                WM_GETICON,
                Some(WPARAM(ICON_SMALL as usize)),
                Some(LPARAM(0)),
            );
            let hicon = if icon.0 != 0 {
                HICON(icon.0 as *mut _)
            } else {
                LoadIconW(None, IDI_APPLICATION)?
            };
            let mut data = Box::new(Data {
                icon: NOTIFYICONDATAW {
                    cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                    hWnd: hwnd,
                    uID: 1,
                    uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
                    uCallbackMessage: CALLBACK,
                    hIcon: hicon,
                    ..Default::default()
                },
                taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
                exit: Arc::new(AtomicBool::new(false)),
                ctx,
            });
            for (slot, value) in data
                .icon
                .szTip
                .iter_mut()
                .zip("Agent-Switch".encode_utf16())
            {
                *slot = value;
            }
            if !SetWindowSubclass(
                hwnd,
                Some(callback),
                SUBCLASS,
                (&mut *data as *mut Data) as usize,
            )
            .as_bool()
            {
                anyhow::bail!("无法安装托盘消息处理器");
            }
            if !Shell_NotifyIconW(NIM_ADD, &data.icon).as_bool() {
                let _ = RemoveWindowSubclass(hwnd, Some(callback), SUBCLASS);
                anyhow::bail!("无法创建系统托盘图标");
            }
            Ok(Self { data })
        }
    }

    pub fn take_exit_request(&self) -> bool {
        self.data.exit.swap(false, Ordering::SeqCst)
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.data.icon);
            let _ = RemoveWindowSubclass(self.data.icon.hWnd, Some(callback), SUBCLASS);
        }
    }
}

fn show(data: &Data) {
    data.ctx
        .send_viewport_cmd(egui::ViewportCommand::Visible(true));
    data.ctx
        .send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    data.ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    unsafe {
        let _ = ShowWindow(data.icon.hWnd, SW_RESTORE);
        let _ = SetForegroundWindow(data.icon.hWnd);
    }
    data.ctx.request_repaint();
}

fn select(data: &Data, selection: usize) {
    if selection == OPEN {
        show(data);
    }
    if selection == EXIT {
        data.exit.store(true, Ordering::SeqCst);
        show(data);
    }
}

unsafe extern "system" fn callback(
    hwnd: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
    _: usize,
    reference: usize,
) -> LRESULT {
    let data = unsafe { &*(reference as *const Data) };
    if message == WM_APP + 74 {
        show(data);
        return LRESULT(0);
    }
    if message == data.taskbar_created && message != 0 {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_ADD, &data.icon);
        }
    }
    if message == CALLBACK {
        match lp.0 as u32 {
            WM_LBUTTONUP | WM_LBUTTONDBLCLK => show(data),
            WM_RBUTTONUP | WM_CONTEXTMENU => {
                if let Ok(selection) = menu(hwnd) {
                    select(data, selection);
                }
            }
            _ => {}
        }
        return LRESULT(0);
    }
    unsafe { DefSubclassProc(hwnd, message, wp, lp) }
}

fn menu(hwnd: HWND) -> Result<usize> {
    unsafe {
        let menu = CreatePopupMenu().context("无法创建托盘菜单")?;
        let result = (|| -> Result<usize> {
            let open: Vec<u16> = crate::i18n::tr("打开界面", "Open window")
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let exit: Vec<u16> = crate::i18n::tr("退出", "Exit")
                .encode_utf16()
                .chain(Some(0))
                .collect();
            AppendMenuW(menu, MF_STRING, OPEN, PCWSTR(open.as_ptr()))?;
            AppendMenuW(menu, MF_STRING, EXIT, PCWSTR(exit.as_ptr()))?;
            let mut point = POINT::default();
            GetCursorPos(&mut point)?;
            let _ = SetForegroundWindow(hwnd);
            let selected = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                Some(0),
                hwnd,
                None,
            )
            .0 as usize;
            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            Ok(selected)
        })();
        let _ = DestroyMenu(menu);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_restores_hidden_window_and_exit_only_requests_confirmation() {
        // Own off-screen fixture, never the real Agent-Switch/Codex window.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("Agent-Switch tray fixture"),
                WS_POPUP,
                -30000,
                -30000,
                1,
                1,
                None,
                None,
                None,
                None,
            )
        }
        .unwrap();
        let tray = Tray::new(hwnd, egui::Context::default()).unwrap();
        assert!(!unsafe { IsWindowVisible(hwnd) }.as_bool());
        unsafe {
            SendMessageW(
                hwnd,
                CALLBACK,
                Some(WPARAM(1)),
                Some(LPARAM(WM_LBUTTONUP as isize)),
            );
        }
        assert!(unsafe { IsWindowVisible(hwnd) }.as_bool());
        assert!(!tray.take_exit_request());
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
            SendMessageW(hwnd, WM_APP + 74, Some(WPARAM(0)), Some(LPARAM(0)));
        }
        assert!(unsafe { IsWindowVisible(hwnd) }.as_bool());
        select(&tray.data, EXIT);
        assert!(tray.take_exit_request());
        assert!(!tray.take_exit_request());
        assert!(unsafe { IsWindow(Some(hwnd)) }.as_bool());
        drop(tray);
        unsafe {
            DestroyWindow(hwnd).unwrap();
        }
    }
}
