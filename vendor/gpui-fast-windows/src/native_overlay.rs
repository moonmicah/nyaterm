//! An owned, nonactivating DirectComposition target. All layout and input state
//! stays in the owner's GPUI Window; this HWND only presents and routes input.
use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
    sync::OnceLock,
};

use anyhow::{Context, Result};
use gpui::{Bounds, Pixels};
use gpui_util::ResultExt;
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{Controls::WM_MOUSELEAVE, Input::KeyboardAndMouse::SetFocus, WindowsAndMessaging::*},
    },
    core::w,
};

pub(crate) const WM_NATIVE_CHILD_MOUSE_DOWN: u32 = WM_APP + 0x3b1;

struct OverlayData {
    owner: HWND,
    regions: RefCell<Vec<RECT>>,
    hwnd: Cell<HWND>,
    input_hwnd: Cell<HWND>,
    attached: Cell<bool>,
    input_attached: Cell<bool>,
}

#[derive(Default)]
struct ThreadHook {
    handle: HHOOK,
    overlays: Vec<Weak<OverlayData>>,
}

thread_local! { static MOUSE_HOOK: RefCell<ThreadHook> = RefCell::default(); }

pub(crate) struct NativeOverlayWindow {
    data: Rc<OverlayData>,
}

impl NativeOverlayWindow {
    pub(crate) fn new(owner: HWND) -> Result<Self> {
        // Legacy child painting requires the owner's redirection surface.
        // Windows rejects changing this creation-time style with SetWindowLong.
        anyhow::ensure!(
            unsafe { GetWindowLongPtrW(owner, GWL_EXSTYLE) } & WS_EX_NOREDIRECTIONBITMAP.0 as isize
                == 0,
            "create the window with WindowOptions::native_child_surfaces enabled"
        );
        static CLASS: OnceLock<bool> = OnceLock::new();
        let registered = CLASS.get_or_init(|| unsafe {
            let class = WNDCLASSW {
                lpfnWndProc: Some(overlay_proc),
                hInstance: GetModuleHandleW(None).unwrap_or_default().into(),
                lpszClassName: w!("GPUI.NativeOverlay"),
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 {
                return false;
            }
            let input_class = WNDCLASSW {
                lpfnWndProc: Some(input_proc),
                lpszClassName: w!("GPUI.NativeOverlayInput"),
                ..class
            };
            RegisterClassW(&input_class) != 0
        });
        anyhow::ensure!(*registered, "register native overlay class");
        let data = Rc::new(OverlayData {
            owner,
            regions: RefCell::default(),
            hwnd: Cell::default(),
            input_hwnd: Cell::default(),
            attached: Cell::new(false),
            input_attached: Cell::new(false),
        });
        let instance = unsafe { GetModuleHandleW(None)? };
        // This extra reference belongs to the HWND until WM_NCDESTROY. The
        // caller's reference also covers failed creation and reentrant messages.
        let raw = Rc::into_raw(data.clone());
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOREDIRECTIONBITMAP
                    | WS_EX_LAYERED
                    | WS_EX_TRANSPARENT,
                w!("GPUI.NativeOverlay"),
                w!(""),
                WS_POPUP,
                0,
                0,
                1,
                1,
                Some(owner),
                None,
                Some(instance.into()),
                Some(raw.cast()),
            )
        };
        let hwnd = match hwnd {
            Ok(hwnd) => hwnd,
            Err(error) => {
                // WM_NCDESTROY releases the reference if WM_NCCREATE ran.
                // A failure before WM_NCCREATE leaves it to this branch.
                if !data.attached.get() {
                    unsafe {
                        drop(Rc::from_raw(raw));
                    }
                }
                return Err(error).context("create native overlay window");
            }
        };
        let overlay = Self { data };
        // HTTRANSPARENT alone does not provide global pointer transparency.
        // Keep the visual popup out of hit testing, and use a separate regioned
        // layered window for interactive GPUI areas only.
        unsafe { SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA) }?;
        let raw_input = Rc::into_raw(overlay.data.clone());
        let input = unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
                w!("GPUI.NativeOverlayInput"),
                w!(""),
                WS_POPUP,
                0,
                0,
                1,
                1,
                Some(owner),
                None,
                Some(instance.into()),
                Some(raw_input.cast()),
            )
        };
        let input = match input {
            Ok(input) => input,
            Err(error) => {
                if !overlay.data.input_attached.get() {
                    unsafe {
                        drop(Rc::from_raw(raw_input));
                    }
                }
                return Err(error).context("create native overlay input window");
            }
        };
        unsafe { SetLayeredWindowAttributes(input, COLORREF(0), 1, LWA_ALPHA) }?;
        overlay.apply_input_region(&[])?;
        MOUSE_HOOK.with(|hook| -> Result<()> {
            let mut hook = hook.borrow_mut();
            if hook.handle.is_invalid() {
                hook.handle = unsafe {
                    SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), Some(instance.into()), 0)
                }?;
            }
            hook.overlays.push(Rc::downgrade(&overlay.data));
            Ok(())
        })?;
        overlay.sync();
        debug_assert_eq!(hwnd, overlay.hwnd());
        Ok(overlay)
    }

    pub(crate) fn hwnd(&self) -> HWND {
        self.data.hwnd.get()
    }

    pub(crate) fn set_regions(&self, regions: &[Bounds<Pixels>], scale: f32) {
        let rectangles: Vec<RECT> = regions
            .iter()
            .map(|bounds| RECT {
                left: (bounds.left().as_f32() * scale).floor() as i32,
                top: (bounds.top().as_f32() * scale).floor() as i32,
                right: (bounds.right().as_f32() * scale).ceil() as i32,
                bottom: (bounds.bottom().as_f32() * scale).ceil() as i32,
            })
            .collect();
        if *self.data.regions.borrow() == rectangles {
            return;
        }
        if self.apply_input_region(&rectangles).log_err().is_none() {
            return;
        }
        *self.data.regions.borrow_mut() = rectangles;
        self.sync();
    }

    fn apply_input_region(&self, rectangles: &[RECT]) -> Result<()> {
        unsafe {
            let region = CreateRectRgn(0, 0, 0, 0);
            anyhow::ensure!(!region.is_invalid(), "create native overlay input region");
            for rect in rectangles {
                let rectangle = CreateRectRgnIndirect(rect);
                if rectangle.is_invalid() {
                    DeleteObject(region.into())
                        .ok()
                        .context("release input region")
                        .log_err();
                    anyhow::bail!("create native overlay input rectangle");
                }
                let combined = CombineRgn(Some(region), Some(region), Some(rectangle), RGN_OR);
                DeleteObject(rectangle.into())
                    .ok()
                    .context("release input rectangle")
                    .log_err();
                if combined == GDI_REGION_TYPE(0) {
                    DeleteObject(region.into())
                        .ok()
                        .context("release input region")
                        .log_err();
                    anyhow::bail!("combine native overlay input region");
                }
            }
            if SetWindowRgn(self.data.input_hwnd.get(), Some(region), true) == 0 {
                DeleteObject(region.into())
                    .ok()
                    .context("release input region")
                    .log_err();
                anyhow::bail!("set native overlay input region");
            }
            // A successful SetWindowRgn transfers the GDI region to Windows.
            Ok(())
        }
    }

    pub(crate) fn sync(&self) {
        unsafe {
            let owner = self.data.owner;
            if !IsWindowVisible(owner).as_bool() || IsIconic(owner).as_bool() {
                let _ = ShowWindow(self.hwnd(), SW_HIDE);
                let _ = ShowWindow(self.data.input_hwnd.get(), SW_HIDE);
                return;
            }
            let mut rect = RECT::default();
            if GetClientRect(owner, &mut rect).is_err() {
                return;
            }
            let mut origin = POINT::default();
            if !ClientToScreen(owner, &mut origin).as_bool() {
                return;
            }
            let _ = SetWindowPos(
                self.hwnd(),
                None,
                origin.x,
                origin.y,
                rect.right,
                rect.bottom,
                SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOOWNERZORDER,
            );
            let _ = ShowWindow(self.hwnd(), SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(
                self.data.input_hwnd.get(),
                Some(HWND_TOP),
                origin.x,
                origin.y,
                rect.right,
                rect.bottom,
                SWP_NOACTIVATE | SWP_NOOWNERZORDER,
            );
            let _ = ShowWindow(
                self.data.input_hwnd.get(),
                if self.data.regions.borrow().is_empty() {
                    SW_HIDE
                } else {
                    SW_SHOWNOACTIVATE
                },
            );
        }
    }
}

impl Drop for NativeOverlayWindow {
    fn drop(&mut self) {
        unsafe {
            if !self.data.input_hwnd.get().is_invalid() {
                DestroyWindow(self.data.input_hwnd.get()).log_err();
            }
            let _ = DestroyWindow(self.hwnd());
        }
        MOUSE_HOOK.with(|hook| {
            let mut hook = hook.borrow_mut();
            hook.overlays.retain(|entry| {
                entry
                    .upgrade()
                    .is_some_and(|data| !Rc::ptr_eq(&data, &self.data))
            });
            if hook.overlays.is_empty() && !hook.handle.is_invalid() {
                unsafe {
                    let _ = UnhookWindowsHookEx(hook.handle);
                }
                hook.handle = HHOOK::default();
            }
        });
    }
}

unsafe extern "system" fn mouse_hook(code: i32, message: WPARAM, parameter: LPARAM) -> LRESULT {
    if code >= 0
        && matches!(
            message.0 as u32,
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
        )
    {
        // ActiveX may create its input HWND on a private thread. A low-level
        // hook runs on this installing UI thread, keeping Rc/GPUI access local
        // while observing clicks destined for those child HWNDs as well.
        let mouse = unsafe { &*(parameter.0 as *const MSLLHOOKSTRUCT) };
        let target = unsafe { WindowFromPoint(mouse.pt) };
        // Release the registry borrow before dispatch: closing a menu can
        // destroy a window or start another native host.
        let overlays = MOUSE_HOOK.with(|hook| {
            hook.borrow()
                .overlays
                .iter()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>()
        });
        for data in overlays {
            if unsafe { IsChild(data.owner, target) }.as_bool() {
                let mut position = mouse.pt;
                if unsafe { ScreenToClient(data.owner, &mut position) }.as_bool() {
                    let packed =
                        ((position.y as u32 & 0xffff) << 16) | (position.x as u32 & 0xffff);
                    unsafe {
                        SendMessageW(
                            data.owner,
                            WM_NATIVE_CHILD_MOUSE_DOWN,
                            Some(message),
                            Some(LPARAM(packed as isize)),
                        );
                    }
                }
                break;
            }
        }
    }
    unsafe { CallNextHookEx(None, code, message, parameter) }
}

unsafe extern "system" fn overlay_proc(
    hwnd: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    unsafe { overlay_message(hwnd, message, wp, lp, false) }
}

unsafe extern "system" fn input_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { overlay_message(hwnd, message, wp, lp, true) }
}

unsafe fn overlay_message(
    hwnd: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
    input: bool,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = unsafe { &*(lp.0 as *const CREATESTRUCTW) };
        let data = create.lpCreateParams as *const OverlayData;
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, data as isize);
        }
        unsafe {
            if input {
                (*data).input_attached.set(true);
                (*data).input_hwnd.set(hwnd);
            } else {
                (*data).attached.set(true);
                (*data).hwnd.set(hwnd);
            }
        }
    }
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const OverlayData;
    if pointer.is_null() {
        return unsafe { DefWindowProcW(hwnd, message, wp, lp) };
    }
    // Forwarded input and activation can synchronously destroy this HWND.
    // Keep its context alive until this invocation of the procedure returns.
    unsafe { Rc::increment_strong_count(pointer) };
    let data = unsafe { Rc::from_raw(pointer) };
    match message {
        WM_NCHITTEST => {
            let mut position = POINT {
                x: lp.0 as i16 as i32,
                y: (lp.0 >> 16) as i16 as i32,
            };
            unsafe {
                let _ = ScreenToClient(data.owner, &mut position);
            }
            let interactive = data.regions.borrow().iter().any(|rect| {
                position.x >= rect.left
                    && position.x < rect.right
                    && position.y >= rect.top
                    && position.y < rect.bottom
            });
            LRESULT(if interactive {
                HTCLIENT as isize
            } else {
                HTTRANSPARENT as isize
            })
        }
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN | WM_LBUTTONUP
        | WM_RBUTTONUP | WM_MBUTTONUP | WM_XBUTTONUP | WM_LBUTTONDBLCLK | WM_RBUTTONDBLCLK
        | WM_MBUTTONDBLCLK | WM_MOUSEMOVE | WM_MOUSEWHEEL | WM_MOUSEHWHEEL | WM_MOUSELEAVE
        | WM_SETCURSOR => {
            if matches!(
                message,
                WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
            ) {
                unsafe {
                    let _ = SetForegroundWindow(data.owner);
                    let _ = SetFocus(Some(data.owner));
                }
            }
            unsafe { SendMessageW(data.owner, message, Some(wp), Some(lp)) }
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            unsafe {
                BeginPaint(hwnd, &mut paint);
                let _ = EndPaint(hwnd, &paint);
            }
            LRESULT(0)
        }
        WM_NCDESTROY => unsafe {
            if input {
                data.input_hwnd.set(HWND::default());
            } else {
                data.hwnd.set(HWND::default());
            }
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Rc::from_raw(pointer));
            DefWindowProcW(hwnd, message, wp, lp)
        },
        _ => unsafe { DefWindowProcW(hwnd, message, wp, lp) },
    }
}

#[cfg(test)]
mod tests {
    use super::NativeOverlayWindow;
    use gpui::{Bounds, point, px, size};
    use windows::{
        Win32::{Foundation::HWND, Graphics::Gdi::*, UI::WindowsAndMessaging::*},
        core::w,
    };

    #[test]
    fn interactive_regions_are_scaled_replaced_and_separate_from_pointer_transparent_visuals() {
        unsafe {
            let owner = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!(""),
                WS_OVERLAPPED,
                0,
                0,
                100,
                100,
                None,
                None,
                None,
                None,
            )
            .expect("create hidden region-test owner");
            let overlay = NativeOverlayWindow::new(owner).expect("create native overlay pair");
            let visual_style = GetWindowLongPtrW(overlay.hwnd(), GWL_EXSTYLE);
            assert_ne!(visual_style & WS_EX_LAYERED.0 as isize, 0);
            assert_ne!(visual_style & WS_EX_TRANSPARENT.0 as isize, 0);
            let region = CreateRectRgn(0, 0, 0, 0);
            assert!(!region.is_invalid());
            overlay.set_regions(
                &[Bounds::new(point(px(10.), px(20.)), size(px(15.), px(10.)))],
                2.,
            );
            assert_ne!(
                GetWindowRgn(overlay.data.input_hwnd.get(), region),
                GDI_REGION_TYPE(0)
            );
            assert!(PtInRegion(region, 25, 45).as_bool());
            assert!(!PtInRegion(region, 5, 5).as_bool());
            overlay.set_regions(&[], 2.);
            assert_eq!(
                GetWindowRgn(overlay.data.input_hwnd.get(), region),
                NULLREGION
            );
            assert!(!PtInRegion(region, 25, 45).as_bool());
            assert!(DeleteObject(region.into()).as_bool());
            DestroyWindow(owner).expect("destroy overlay owner");
            assert!(overlay.hwnd().is_invalid());
            assert!(overlay.data.input_hwnd.get().is_invalid());
            drop(overlay);
        }
    }

    #[test]
    fn native_overlay_rejects_an_owner_created_without_child_redirection() {
        unsafe {
            let owner = CreateWindowExW(
                WS_EX_NOREDIRECTIONBITMAP,
                w!("STATIC"),
                w!(""),
                WS_OVERLAPPED,
                0,
                0,
                100,
                100,
                None,
                None,
                None,
                None,
            )
            .expect("create hidden unredirected owner");
            let error = NativeOverlayWindow::new(owner)
                .err()
                .expect("unredirected owner must be rejected");
            assert!(error.to_string().contains("native_child_surfaces"));
            DestroyWindow(owner).expect("destroy unredirected owner");
        }
    }

    #[test]
    fn owner_destruction_invalidates_the_overlay_before_its_rust_owner_drops() {
        unsafe {
            let owner = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!(""),
                WS_OVERLAPPED,
                0,
                0,
                100,
                100,
                None,
                None,
                None,
                None,
            )
            .expect("create hidden test owner");
            let overlay = NativeOverlayWindow::new(owner).expect("create owned overlay");
            assert_ne!(overlay.hwnd(), HWND::default());
            DestroyWindow(owner).expect("destroy owner and owned window");
            assert_eq!(overlay.hwnd(), HWND::default());
            drop(overlay);
        }
    }
}
