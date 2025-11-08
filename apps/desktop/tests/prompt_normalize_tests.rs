use desktop::prompt_normalize::normalize_prompt_in_place;
use serde_json::json;

fn fallback_samplers() -> Vec<String> {
    [
        "euler",
        "euler_ancestral",
        "heun",
        "dpmpp_2m",
        "dpmpp_2m_sde",
        "dpmpp_3m_sde",
        "k_lms",
        "ddim",
    ]
    .into_iter()
    .map(|s| s.to_string())
    .collect()
}

#[test]
fn normalizes_ksampler_array_nodes() {
    let mut prompt = json!({
        "nodes": [
            {
                "class_type": "KSampler",
                "inputs": {
                    "denoise": "simple",
                    "scheduler": "uni_pc",
                    "steps": "randomize",
                    "sampler_name": 4
                }
            }
        ]
    });

    normalize_prompt_in_place(&mut prompt, &fallback_samplers()).unwrap();

    let inputs = &prompt["nodes"][0]["inputs"];
    assert_eq!(inputs["scheduler"].as_str().unwrap(), "karras");
    assert_eq!(inputs["steps"].as_i64().unwrap(), 22);
    assert_eq!(inputs["sampler_name"].as_str().unwrap(), "dpmpp_2m_sde");
    assert!(
        (inputs["denoise"].as_f64().unwrap() - 1.0).abs() < f64::EPSILON,
        "expected denoise to be coerced to 1.0"
    );
}

#[test]
fn normalizes_ksampler_map_nodes() {
    let mut prompt = json!({
        "3": {
            "class_type": "KSamplerAdvanced",
            "inputs": {
                "denoise": "0.35",
                "scheduler": "ddim",
                "steps": "30",
                "sampler_name": "euler"
            }
        }
    });

    normalize_prompt_in_place(&mut prompt, &fallback_samplers()).unwrap();

    let inputs = &prompt["3"]["inputs"];
    assert!(
        (inputs["denoise"].as_f64().unwrap() - 0.35).abs() < f64::EPSILON,
        "expected denoise to parse numeric strings"
    );
    assert_eq!(inputs["scheduler"].as_str().unwrap(), "ddim_uniform");
    assert_eq!(inputs["steps"].as_i64().unwrap(), 30);
    assert_eq!(inputs["sampler_name"].as_str().unwrap(), "euler");
}
