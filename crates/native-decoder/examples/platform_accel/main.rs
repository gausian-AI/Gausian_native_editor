#[cfg(feature = "gstreamer")]
fn main() -> anyhow::Result<()> {
    use anyhow::Context;
    use gst::glib;
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use native_decoder::{build_platform_accelerated_pipeline, select_best_decoder};

    let Some(uri) = std::env::args().nth(1) else {
        eprintln!("Usage: cargo run -p native-decoder --features gstreamer --example platform_accel <video-path>");
        std::process::exit(1);
    };

    let selection = select_best_decoder()?;
    println!(
        "Selected decoder: {} ({})",
        selection.factory_name,
        if selection.is_hardware {
            "hardware acceleration"
        } else {
            "software fallback"
        }
    );

    let pipeline = build_platform_accelerated_pipeline(&uri)?;
    let bus = pipeline.bus().context("pipeline missing message bus")?;

    let main_loop = glib::MainLoop::new(None, false);
    let loop_ref = main_loop.clone();

    bus.add_watch(move |_, msg| {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => {
                println!("Playback completed (EOS).");
                loop_ref.quit();
                glib::Continue(false)
            }
            MessageView::Error(err) => {
                eprintln!(
                    "Playback error from {}: {} ({:?})",
                    err.src()
                        .map(|s| s.path_string())
                        .unwrap_or_else(|| "unknown".to_string()),
                    err.error(),
                    err.debug()
                );
                loop_ref.quit();
                glib::Continue(false)
            }
            _ => glib::Continue(true),
        }
    })
    .context("attach bus watch")?;

    pipeline
        .set_state(gst::State::Playing)
        .context("set pipeline to Playing")?;
    println!("Pipeline set to Playing; waiting for EOS or Error.");

    main_loop.run();

    pipeline
        .set_state(gst::State::Null)
        .context("set pipeline to Null")?;
    Ok(())
}

#[cfg(not(feature = "gstreamer"))]
fn main() {
    eprintln!("Enable the `gstreamer` feature to build this example.");
}
