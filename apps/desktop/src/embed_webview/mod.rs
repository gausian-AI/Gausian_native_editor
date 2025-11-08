// Cross-platform embedding API for showing a webview over an egui panel rect.

#[derive(Clone, Copy, Debug, Default)]
pub struct RectPx {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

pub trait WebViewHost {
    fn navigate(&mut self, url: &str);
    fn set_rect(&mut self, rect: RectPx);
    fn set_visible(&mut self, vis: bool);
    fn is_visible(&self) -> bool;
    fn reload(&mut self);
    fn close(&mut self);
    fn focus(&mut self) {}
    fn paste_from_clipboard(&mut self) {}
    fn insert_text(&mut self, _text: &str) {}
}

#[cfg(all(target_os = "macos", feature = "embed-webview"))]
mod macos;

#[cfg(all(target_os = "macos", feature = "embed-webview"))]
pub use macos::create_host as create_platform_host;

#[cfg(not(all(target_os = "macos", feature = "embed-webview")))]
pub fn create_platform_host() -> Option<Box<dyn WebViewHost>> {
    None
}
