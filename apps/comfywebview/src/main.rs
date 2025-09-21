use clap::Parser;
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::EventLoop;
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

#[derive(Parser, Debug)]
#[command(author, version, about = "Minimal WebView window for ComfyUI", long_about = None)]
struct Args {
    /// URL to load (e.g., http://127.0.0.1:8188)
    #[arg(long)]
    url: String,
    /// Window title
    #[arg(long, default_value = "ComfyUI")]
    title: String,
    /// Window width
    #[arg(long, default_value_t = 1280)]
    width: u32,
    /// Window height
    #[arg(long, default_value_t = 800)]
    height: u32,
}

fn main() {
    let args = Args::parse();

    let mut event_loop: EventLoop<()> = EventLoop::new();
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        event_loop.set_activation_policy(ActivationPolicy::Regular);
    }
    let window = WindowBuilder::new()
        .with_title(args.title)
        .with_inner_size(LogicalSize::new(
            args.width as f64,
            args.height as f64,
        ))
        .build(&event_loop)
        .expect("failed to create window");

    let _webview = WebViewBuilder::new(&window)
        .with_url(&args.url)
        .build()
        .expect("failed to build webview");

    use tao::event_loop::ControlFlow;
    event_loop.run(move |event, _window_target, control_flow| {
        *control_flow = ControlFlow::Wait;
        if let Event::WindowEvent { event, .. } = event {
            match event {
                WindowEvent::CloseRequested => *control_flow = ControlFlow::Exit,
                _ => {}
            }
        }
    });
}
