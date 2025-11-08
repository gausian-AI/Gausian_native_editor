use once_cell::sync::OnceCell;
use serde::Deserialize;

static REQUIREMENTS: OnceCell<Vec<RequirementItem>> = OnceCell::new();
static REQUIREMENTS_PROMPT: OnceCell<String> = OnceCell::new();
static PLOT_TYPES: OnceCell<Vec<PlotTypeSpec>> = OnceCell::new();
static PLOT_TYPES_PROMPT: OnceCell<String> = OnceCell::new();
static SCREENPLAY_FORMAT_PROMPT: OnceCell<String> = OnceCell::new();

const REQUIREMENTS_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../formats/screenplay_requirements.json"
));
const PLOT_TYPES_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../formats/plot_types.json"
));
const ESSENTIAL_REQUIREMENTS: &[&str] = &[
    "core_metadata.duration_minutes",
    "core_metadata.genre",
    "narrative_context.setting",
    "narrative_context.theme",
    "narrative_context.tone",
];

#[derive(Debug, Clone)]
pub struct RequirementItem {
    pub id: String,
    pub label: String,
    pub hint: String,
    pub category: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlotTypeSpec {
    #[serde(rename = "plot_structure")]
    pub structure: String,
    #[serde(rename = "plot_progression")]
    pub progression: Vec<String>,
    pub narrative_goal: String,
    #[serde(default)]
    pub intended_emotional_impact: Vec<String>,
    #[serde(default)]
    pub best_genres: Vec<String>,
    #[serde(default)]
    pub best_media: Vec<String>,
}

pub fn screenplay_requirements() -> &'static [RequirementItem] {
    REQUIREMENTS.get_or_init(build_requirements)
}

pub fn screenplay_requirements_prompt() -> &'static str {
    REQUIREMENTS_PROMPT.get_or_init(|| {
        let essentials: Vec<&RequirementItem> = screenplay_requirements()
            .iter()
            .filter(|item| ESSENTIAL_REQUIREMENTS.contains(&item.id.as_str()))
            .collect();
        let mut out = String::from(
            "Before drafting, collect these essentials (skip others unless the user volunteers more detail):\n",
        );
        for item in essentials {
            out.push_str(&format!("• {} — {}\n", item.label, item.hint));
        }
        out.push_str(
            "\nIf the user has not yet given a target runtime, explicitly ask \
“What runtime in minutes should we target?” and wait for that answer before \
offering outlines or drafts.\n",
        );
        out
    })
}

pub fn plot_types() -> &'static [PlotTypeSpec] {
    PLOT_TYPES
        .get_or_init(|| serde_json::from_str(PLOT_TYPES_JSON).expect("plot_types.json to be valid"))
}

pub fn plot_types_prompt() -> &'static str {
    PLOT_TYPES_PROMPT.get_or_init(|| {
        let mut out = String::from("Recommended plot structures you can suggest or adapt:\n");
        for plot in plot_types() {
            out.push_str(&format!("\n- {}:\n", plot.structure));
            if !plot.progression.is_empty() {
                out.push_str("  Progression:\n");
                for beat in &plot.progression {
                    out.push_str(&format!("    • {}\n", beat));
                }
            }
            out.push_str(&format!("  Narrative goal: {}\n", plot.narrative_goal));
            if !plot.intended_emotional_impact.is_empty() {
                out.push_str("  Emotional impact: ");
                out.push_str(&plot.intended_emotional_impact.join(", "));
                out.push('\n');
            }
        }
        out
    })
}

pub fn screenplay_format_prompt() -> &'static str {
    SCREENPLAY_FORMAT_PROMPT.get_or_init(|| {
        String::from(
            "Return the draft as JSON only (no Markdown, prose, or explanation). \
Use this structure and field ordering:\n\
{\n\
  \"title\": \"<string>\",\n\
  \"genre\": \"<string>\",\n\
  \"duration\": <number>,\n\
  \"synopsis\": \"<string>\",\n\
  \"acts\": [\n\
    {\n\
      \"id\": <integer>,\n\
      \"title\": \"<string>\",\n\
      \"summary\": \"<string>\",\n\
      \"shots\": [\n\
        {\n\
          \"id\": <integer>,\n\
          \"title\": \"<string>\",\n\
          \"visual_description\": \"<string>\",\n\
          \"location\": \"<string>\",\n\
          \"characters\": [\"<string>\", \"...\"] ,\n\
          \"duration\": <number>,\n\
          \"camera\": {\n\
            \"movement\": \"<string>\",\n\
            \"angle\": \"<string>\"\n\
          },\n\
          \"sound\": {\n\
            \"music\": \"<string>\",\n\
            \"fx\": \"<string>\",\n\
            \"dialogue\": [\n\
              {\n\
                \"character\": \"<string>\",\n\
                \"line\": \"<string>\"\n\
              }\n\
            ]\n\
          },\n\
          \"workflow\": {\n\
            \"key\": \"<workflow-key>\",\n\
            \"inputs\": {\n\
              \"<input_key>\": \"<value>\"\n\
            }\n\
          }\n\
        }\n\
      ]\n\
    }\n\
  ]\n\
}\n\
 Populate every field with story-specific content. Use empty strings or arrays when information does not apply. \
 Duration should be the target runtime in minutes, while per-shot durations are seconds. \
 Every shot must include the \"workflow\" object pointing to one of the catalog keys above. \
 Ensure the JSON parses without additional text.",
        )
    })
}

fn build_requirements() -> Vec<RequirementItem> {
    let root: serde_json::Value =
        serde_json::from_str(REQUIREMENTS_JSON).expect("screenplay_requirements.json to be valid");
    let mut items = Vec::new();
    if let serde_json::Value::Object(map) = root {
        for (category, value) in &map {
            if let serde_json::Value::Object(sections) = value {
                for (section, value) in sections {
                    let section_obj = section.as_str();
                    flatten_requirements(
                        &format!("{category}.{section_obj}"),
                        section_obj,
                        category,
                        value,
                        &mut items,
                    );
                }
            }
        }
    }
    items
}

fn flatten_requirements(
    path: &str,
    label: &str,
    category: &str,
    value: &serde_json::Value,
    out: &mut Vec<RequirementItem>,
) {
    match value {
        serde_json::Value::String(hint) => {
            out.push(RequirementItem {
                id: path.to_string(),
                label: prettify_label(label),
                hint: hint.to_string(),
                category: prettify_label(category),
            });
        }
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                let key_str = key.as_str();
                let next_path = format!("{path}.{key_str}");
                flatten_requirements(&next_path, key_str, category, child, out);
            }
        }
        serde_json::Value::Array(list) => {
            if let Some(first) = list.first() {
                match first {
                    serde_json::Value::String(hint) => {
                        out.push(RequirementItem {
                            id: format!("{path}[]"),
                            label: prettify_label(label),
                            hint: hint.to_string(),
                            category: prettify_label(category),
                        });
                    }
                    serde_json::Value::Object(obj) => {
                        for (key, child) in obj {
                            let next_path = format!("{path}[].{key}");
                            flatten_requirements(&next_path, key, category, child, out);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn prettify_label(raw: &str) -> String {
    raw.replace('_', " ")
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
