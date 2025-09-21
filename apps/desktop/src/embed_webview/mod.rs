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
    fn set_devtools(&mut self, enabled: bool);
    // Best-effort: try to open developer inspector if supported. Returns true on success.
    fn open_inspector(&mut self) -> bool;
    fn close(&mut self);
}

#[cfg(all(target_os = "macos", feature = "embed-webview"))]
mod macos;

#[cfg(all(target_os = "macos", feature = "embed-webview"))]
pub use macos::create_host as create_platform_host;

#[cfg(not(all(target_os = "macos", feature = "embed-webview")))]
pub fn create_platform_host() -> Option<Box<dyn WebViewHost>> { None }
