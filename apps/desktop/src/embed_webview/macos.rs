#![cfg(target_os = "macos")]

use super::{RectPx, WebViewHost};

#[link(name = "WebKit", kind = "framework")]
extern "C" {}

use cocoa::appkit::{NSApp, NSView, NSWindow};
use cocoa::base::{id, nil, NO, YES};
use cocoa::foundation::{NSPoint, NSRect, NSSize, NSString};
use objc::{class, msg_send, sel, sel_impl};
use serde_json;

pub struct MacWebViewHost {
    webview: id,
    parent_view: id,
    visible: bool,
    last_url: Option<String>,
}

impl MacWebViewHost {
    unsafe fn parent_content_view() -> Option<id> {
        let app = NSApp();
        if app == nil {
            return None;
        }
        let mut window: id = msg_send![app, keyWindow];
        if window == nil {
            window = msg_send![app, mainWindow];
        }
        if window != nil {
            let content: id = msg_send![window, contentView];
            if content != nil {
                return Some(content);
            }
        }
        // Fallback: iterate all windows and pick the first visible one with a content view.
        let windows: id = msg_send![app, windows];
        if windows != nil {
            let count: i64 = msg_send![windows, count];
            let mut i: i64 = 0;
            while i < count {
                let w: id = msg_send![windows, objectAtIndex: i as u64];
                let is_visible: bool = msg_send![w, isVisible];
                let content: id = msg_send![w, contentView];
                if is_visible && content != nil {
                    return Some(content);
                }
                i += 1;
            }
        }
        None
    }

    unsafe fn content_height(view: id) -> f64 {
        let frame: NSRect = msg_send![view, frame];
        frame.size.height as f64
    }

    unsafe fn make_wkwebview(frame: NSRect) -> Option<id> {
        let cfg: id = msg_send![class!(WKWebViewConfiguration), new];
        if cfg == nil {
            return None;
        }
        let webview: id = msg_send![class!(WKWebView), alloc];
        if webview == nil {
            return None;
        }
        let webview: id = msg_send![webview, initWithFrame: frame configuration: cfg];
        if webview == nil {
            return None;
        }
        Some(webview)
    }

    unsafe fn load_url(webview: id, url: &str) {
        let ns_url_str = NSString::alloc(nil).init_str(url);
        let nsurl: id = msg_send![class!(NSURL), URLWithString: ns_url_str];
        if nsurl == nil {
            return;
        }
        let req: id = msg_send![class!(NSURLRequest), requestWithURL: nsurl];
        if req == nil {
            return;
        }
        let _: () = msg_send![webview, loadRequest: req];
    }
}

impl WebViewHost for MacWebViewHost {
    fn navigate(&mut self, url: &str) {
        self.last_url = Some(url.to_string());
        unsafe { Self::load_url(self.webview, url) }
    }
    fn set_rect(&mut self, rect: RectPx) {
        unsafe {
            // Content-view points with top-left origin (egui). Flip to AppKit bottom-left.
            let frame: NSRect = msg_send![self.parent_view, frame];
            let flipped: bool = msg_send![self.parent_view, isFlipped];
            let x = rect.x.max(0) as f64;
            let mut w = rect.w.max(0) as f64;
            let h = rect.h.max(0) as f64;
            let y_top = rect.y.max(0) as f64;
            let y = if flipped {
                y_top
            } else {
                (frame.size.height as f64 - (y_top + h)).max(0.0)
            };
            w = w.min(frame.size.width as f64);
            let view_rect = NSRect::new(NSPoint::new(x, y), NSSize::new(w, h));
            let _: () = msg_send![self.webview, setFrame: view_rect];
            let _: () = msg_send![self.parent_view, addSubview: self.webview positioned: 1 /* NSWindowAbove */ relativeTo: nil];
        }
    }
    fn set_visible(&mut self, vis: bool) {
        unsafe {
            let _: () = msg_send![self.webview, setHidden: if vis { NO } else { YES }];
        }
        self.visible = vis;
    }
    fn reload(&mut self) {
        unsafe {
            let _: () = msg_send![self.webview, reload];
        }
    }
    fn is_visible(&self) -> bool {
        self.visible
    }
    fn close(&mut self) {
        unsafe {
            let _: () = msg_send![self.webview, removeFromSuperview];
        }
        self.visible = false;
    }
    fn focus(&mut self) {
        unsafe {
            let window: id = msg_send![self.parent_view, window];
            if window != nil {
                let _: () = msg_send![window, makeFirstResponder: self.webview];
            }
        }
    }
    fn paste_from_clipboard(&mut self) {
        self.focus();
        unsafe {
            let _: () = msg_send![self.webview, paste: nil];
        }
    }
    fn insert_text(&mut self, text: &str) {
        self.focus();
        let Ok(json_text) = serde_json::to_string(text) else {
            return;
        };
        let script = format!(
            "(function() {{
                const text = {json};
                const active = document.activeElement;
                if (!active) return;
                if (typeof active.value === 'string' && active !== document.body) {{
                    const start = active.selectionStart ?? active.value.length;
                    const end = active.selectionEnd ?? start;
                    const value = active.value;
                    active.value = value.slice(0, start) + text + value.slice(end);
                    const pos = start + text.length;
                    if (typeof active.setSelectionRange === 'function') {{
                        active.setSelectionRange(pos, pos);
                    }}
                    active.dispatchEvent(new Event('input', {{ bubbles: true }}));
                    active.dispatchEvent(new Event('change', {{ bubbles: true }}));
                }} else {{
                    document.execCommand('insertText', false, text);
                }}
            }})();",
            json = json_text,
        );
        unsafe {
            let ns_script = NSString::alloc(nil).init_str(&script);
            let _: () =
                msg_send![self.webview, evaluateJavaScript: ns_script completionHandler: nil];
        }
    }
}

pub fn create_host() -> Option<Box<dyn WebViewHost>> {
    unsafe {
        let Some(parent) = MacWebViewHost::parent_content_view() else {
            return None;
        };
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(100.0, 100.0));
        let Some(webview) = MacWebViewHost::make_wkwebview(frame) else {
            return None;
        };
        // Ensure we add above the wgpu surface view
        let _: () = msg_send![parent, addSubview: webview positioned: 1 /* NSWindowAbove */ relativeTo: nil];
        let mut host = MacWebViewHost {
            webview,
            parent_view: parent,
            visible: false,
            last_url: None,
        };
        host.set_visible(true);
        host.focus();
        Some(Box::new(host))
    }
}
