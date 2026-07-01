//! Integration test for the Twelve Labs Pegasus footage-analysis helper.
//!
//! The live API call is gated on `TWELVELABS_API_KEY`. Without the key the test
//! is skipped, so it never breaks CI or local builds for contributors who do not
//! have credentials. Grab a free key at https://twelvelabs.io.
//!
//! Run it with:
//! ```bash
//! TWELVELABS_API_KEY=tlk_... cargo test -p desktop --test footage_analysis_tests -- --nocapture
//! ```

use desktop::footage_analysis::{FootageAnalyzer, PegasusConfig, VideoSource};

/// A small, direct-link public sample clip the Twelve Labs API can fetch.
const SAMPLE_VIDEO_URL: &str =
    "https://test-videos.co.uk/vids/bigbuckbunny/mp4/h264/360/Big_Buck_Bunny_360_10s_1MB.mp4";

#[test]
fn analyzes_public_sample_when_key_present() {
    let Ok(api_key) = std::env::var("TWELVELABS_API_KEY") else {
        eprintln!("skipping: TWELVELABS_API_KEY not set");
        return;
    };
    if api_key.trim().is_empty() {
        eprintln!("skipping: TWELVELABS_API_KEY is empty");
        return;
    }

    let analyzer = FootageAnalyzer::new(PegasusConfig::new(api_key))
        .expect("analyzer should build with a key");

    let result = analyzer
        .analyze(
            VideoSource::url(SAMPLE_VIDEO_URL),
            "Describe this footage in one sentence.",
        )
        .expect("Pegasus analysis should succeed for a reachable public clip");

    assert!(
        !result.text.trim().is_empty(),
        "expected non-empty analysis text, got: {:?}",
        result
    );
    eprintln!("Pegasus analysis: {}", result.text);
}
