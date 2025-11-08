use anyhow::{anyhow, Result};
use serde_json::Value;

const ALLOWED_SCHEDULERS: &[&str] = &[
    "simple",
    "sgm_uniform",
    "karras",
    "exponential",
    "ddim_uniform",
    "beta",
    "normal",
    "linear_quadratic",
    "kl_optimal",
];

fn map_scheduler_alias(raw: &str) -> String {
    let lower = raw.trim().to_ascii_lowercase();
    match lower.as_str() {
        "uni_pc" | "unipc" | "unipc_snr" => "karras".to_string(),
        "ddim" => "ddim_uniform".to_string(),
        other => other.to_string(),
    }
}

fn coerce_float(value: &Value, field: &str) -> Result<f32> {
    match value {
        Value::Number(n) => {
            let raw = n
                .as_f64()
                .ok_or_else(|| anyhow!("{field} must be a floating point number"))?;
            let f = raw as f32;
            if !f.is_finite() {
                Err(anyhow!("{field} must be finite"))
            } else {
                Ok(f.clamp(0.0, 1.0))
            }
        }
        Value::String(s) => {
            let trimmed = s.trim();
            let parsed: f32 = trimmed
                .parse()
                .map_err(|_| anyhow!("{field} must be a float, got '{s}'"))?;
            if !parsed.is_finite() {
                Err(anyhow!("{field} must be finite"))
            } else {
                Ok(parsed.clamp(0.0, 1.0))
            }
        }
        _ => Err(anyhow!("{field} must be a float or string")),
    }
}

fn coerce_int(value: &Value, field: &str) -> Result<i32> {
    match value {
        Value::Number(n) => {
            let raw = n
                .as_i64()
                .ok_or_else(|| anyhow!("{field} must be an integer"))?;
            i32::try_from(raw).map_err(|_| anyhow!("{field} out of range"))
        }
        Value::String(s) => {
            let trimmed = s.trim();
            trimmed
                .parse::<i32>()
                .map_err(|_| anyhow!("{field} must be an int, got '{s}'"))
        }
        _ => Err(anyhow!("{field} must be an int or string")),
    }
}

fn random_steps() -> i32 {
    22
}

fn normalize_ksampler_inputs(inputs: &mut Value, sampler_names: &[String]) -> Result<()> {
    let obj = inputs
        .as_object_mut()
        .ok_or_else(|| anyhow!("KSampler.inputs must be an object"))?;

    if let Some(value) = obj.get("denoise") {
        let normalized = if value
            .as_str()
            .map(|s| s.eq_ignore_ascii_case("simple"))
            .unwrap_or(false)
        {
            1.0_f32
        } else {
            coerce_float(value, "denoise")?
        };
        obj.insert("denoise".into(), Value::from(normalized));
    }

    if let Some(value) = obj.get("scheduler") {
        let raw = value
            .as_str()
            .ok_or_else(|| anyhow!("scheduler must be a string"))?;
        let mapped = map_scheduler_alias(raw);
        let valid = ALLOWED_SCHEDULERS
            .iter()
            .find(|candidate| candidate.eq_ignore_ascii_case(mapped.as_str()));
        let scheduler = valid
            .copied()
            .ok_or_else(|| anyhow!("scheduler '{}' not in allowed list", mapped))?;
        obj.insert("scheduler".into(), Value::String(scheduler.to_string()));
    }

    if let Some(value) = obj.get("steps") {
        let steps = if value
            .as_str()
            .map(|s| s.eq_ignore_ascii_case("randomize"))
            .unwrap_or(false)
        {
            random_steps()
        } else {
            coerce_int(value, "steps")?
        };
        obj.insert("steps".into(), Value::from(steps));
    }

    if let Some(value) = obj.get("sampler_name") {
        if let Some(index) = value.as_u64() {
            let sampler = sampler_names.get(index as usize).ok_or_else(|| {
                anyhow!(
                    "sampler_name index {} out of range ({} entries)",
                    index,
                    sampler_names.len()
                )
            })?;
            obj.insert("sampler_name".into(), Value::String(sampler.clone()));
        }
    }

    Ok(())
}

fn normalize_node(node: &mut Value, sampler_names: &[String]) -> Result<()> {
    let Some(obj) = node.as_object_mut() else {
        return Ok(());
    };
    let class_type = obj
        .get("class_type")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if matches!(class_type, "KSampler" | "KSamplerAdvanced") {
        if let Some(inputs) = obj.get_mut("inputs") {
            normalize_ksampler_inputs(inputs, sampler_names)?;
        }
    }
    Ok(())
}

fn normalize_nodes_collection(nodes: &mut Value, sampler_names: &[String]) -> Result<()> {
    match nodes {
        Value::Array(array) => {
            for node in array.iter_mut() {
                normalize_node(node, sampler_names)?;
            }
        }
        Value::Object(map) => {
            for value in map.values_mut() {
                normalize_node(value, sampler_names)?;
            }
        }
        _ => return Err(anyhow!("nodes collection must be array or object")),
    }
    Ok(())
}

pub fn normalize_prompt_in_place(prompt: &mut Value, sampler_names: &[String]) -> Result<()> {
    if let Some(nodes) = prompt.get_mut("nodes") {
        normalize_nodes_collection(nodes, sampler_names)?;
        return Ok(());
    }

    match prompt {
        Value::Object(map) => {
            for value in map.values_mut() {
                normalize_node(value, sampler_names)?;
            }
            Ok(())
        }
        Value::Array(array) => {
            for node in array.iter_mut() {
                normalize_node(node, sampler_names)?;
            }
            Ok(())
        }
        _ => Err(anyhow!("Unrecognized prompt structure")),
    }
}
