#![cfg(target_os = "macos")]

use super::{RectPx, WebViewHost};

#[link(name = "WebKit", kind = "framework")]
extern "C" {}

use cocoa::appkit::{NSApp, NSView, NSWindow};
use cocoa::base::{id, nil, NO, YES};
use cocoa::foundation::{NSPoint, NSRect, NSSize, NSString};
use objc::class;
use objc::runtime::Object;
use objc::{msg_send, sel, sel_impl};

pub struct MacWebViewHost {
    webview: id,
    parent_view: id,
    visible: bool,
    devtools: bool,
    last_url: Option<String>,
}

impl MacWebViewHost {
    unsafe fn parent_content_view() -> Option<id> {
        let app = NSApp();
        if app == nil { return None; }
        let mut window: id = msg_send![app, keyWindow];
        if window == nil { window = msg_send![app, mainWindow]; }
        if window != nil {
            let content: id = msg_send![window, contentView];
            if content != nil { return Some(content); }
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
                if is_visible && content != nil { return Some(content); }
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
        if cfg == nil { return None; }
        let webview: id = msg_send![class!(WKWebView), alloc];
        if webview == nil { return None; }
        let webview: id = msg_send![webview, initWithFrame: frame configuration: cfg];
        if webview == nil { return None; }
        Some(webview)
    }

    unsafe fn load_url(webview: id, url: &str) {
        let ns_url_str = NSString::alloc(nil).init_str(url);
        let nsurl: id = msg_send![class!(NSURL), URLWithString: ns_url_str];
        if nsurl == nil { return; }
        let req: id = msg_send![class!(NSURLRequest), requestWithURL: nsurl];
        if req == nil { return; }
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
            let y = if flipped { y_top } else { (frame.size.height as f64 - (y_top + h)).max(0.0) };
            w = w.min(frame.size.width as f64);
            let view_rect = NSRect::new(NSPoint::new(x, y), NSSize::new(w, h));
            let _: () = msg_send![self.webview, setFrame: view_rect];
            let _: () = msg_send![self.parent_view, addSubview: self.webview positioned: 1 /* NSWindowAbove */ relativeTo: nil];
        }
    }
    fn set_visible(&mut self, vis: bool) {
        unsafe { let _: () = msg_send![self.webview, setHidden: if vis { NO } else { YES }]; }
        self.visible = vis;
    }
    fn reload(&mut self) {
        unsafe { let _: () = msg_send![self.webview, reload]; }
    }
    fn set_devtools(&mut self, enabled: bool) {
        if self.devtools == enabled { return; }
        self.devtools = enabled;
        unsafe {
            // Recreate with developer extras setting
            let old_frame: NSRect = msg_send![self.webview, frame];
            let _: () = msg_send![self.webview, removeFromSuperview];
            let cfg: id = msg_send![class!(WKWebViewConfiguration), new];
            if cfg != nil {
                let key: id = NSString::alloc(nil).init_str("developerExtrasEnabled");
                let prefs: id = msg_send![cfg, preferences];
                if prefs != nil { let _: () = msg_send![prefs, setValue: if enabled { YES } else { NO } forKey: key]; }
                let webview_alloc: id = msg_send![class!(WKWebView), alloc];
                if webview_alloc != nil {
                    let webview_new: id = msg_send![webview_alloc, initWithFrame: old_frame configuration: cfg];
                    if webview_new != nil {
                        self.webview = webview_new;
                        let _: () = msg_send![self.parent_view, addSubview: self.webview positioned: 1 relativeTo: nil];
                        if let Some(url) = self.last_url.clone() { Self::load_url(self.webview, &url); }
                        let _: () = msg_send![self.webview, setHidden: if self.visible { NO } else { YES }];
                    }
                }
            }
        }
    }
    fn open_inspector(&mut self) -> bool {
        // Best-effort: try private APIs if present; otherwise advise user to right-click → Inspect
        unsafe {
            // Ensure devtools are enabled
            if !self.devtools { self.set_devtools(true); }
            // Try KVC to get inspector object and call show
            let key: id = NSString::alloc(nil).init_str("inspector");
            let inspector: id = msg_send![self.webview, valueForKey: key];
            if inspector != nil {
                let _: () = msg_send![inspector, show];
                return true;
            }
            // Try direct selector on webview
            let can1: bool = msg_send![self.webview, respondsToSelector: sel!(showInspector:)];
            if can1 { let _: () = msg_send![self.webview, showInspector: nil]; return true; }
            let can2: bool = msg_send![self.webview, respondsToSelector: sel!(showWebInspector:)];
            if can2 { let _: () = msg_send![self.webview, showWebInspector: nil]; return true; }
        }
        false
    }
    fn is_visible(&self) -> bool { self.visible }
    fn close(&mut self) {
        unsafe { let _: () = msg_send![self.webview, removeFromSuperview]; }
        self.visible = false;
    }
}

pub fn create_host() -> Option<Box<dyn WebViewHost>> {
    unsafe {
        let Some(parent) = MacWebViewHost::parent_content_view() else { return None; };
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(100.0, 100.0));
        let Some(webview) = MacWebViewHost::make_wkwebview(frame) else { return None; };
        // Ensure we add above the wgpu surface view
        let _: () = msg_send![parent, addSubview: webview positioned: 1 /* NSWindowAbove */ relativeTo: nil];
        let mut host = MacWebViewHost { webview, parent_view: parent, visible: false, devtools: false, last_url: None };
        host.set_visible(true);
        Some(Box::new(host))
    }
}
