//! Ollama discovery: what is installed on this machine, asked of the daemon itself.

use std::time::Duration;

pub const DEFAULT_HOST: &str = "http://127.0.0.1:11434";

pub fn base_url(host: &str) -> String {
    format!("{}/v1", host.trim_end_matches('/'))
}

const TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Models {
    pub text: Vec<String>,
    /// Models served from `ollama.com` (`remote_host`, or a `-cloud` tag). Dictation text
    /// would leave the machine, so the wizard lists them apart and confirms before writing one.
    pub cloud: Vec<String>,
    /// Models whose name says whisper. Ollama cannot run them (a whisper GGUF is built for
    /// whisper.cpp; `/v1/audio/transcriptions` answers 500). Kept so the wizard can say so.
    pub unusable_speech: Vec<String>,
}

impl Models {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.cloud.is_empty() && self.unusable_speech.is_empty()
    }
}

fn is_speech(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("whisper") || n.contains("faster-distil") || n.contains("speech-to-text")
}

fn is_embedding(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("embed") || n.contains("bge-") || n.contains("minilm")
}

pub fn classify(entries: &[(String, bool)]) -> Models {
    let mut m = Models::default();
    for (name, cloud) in entries {
        if is_speech(name) {
            m.unusable_speech.push(name.clone());
        } else if is_embedding(name) {
            continue;
        } else if *cloud {
            m.cloud.push(name.clone());
        } else {
            m.text.push(name.clone());
        }
    }
    for list in [&mut m.text, &mut m.cloud, &mut m.unusable_speech] {
        list.sort();
        list.dedup();
    }
    m
}

fn names_in(body: &str) -> Vec<(String, bool)> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    v.get("models")
        .and_then(|m| m.as_array())
        .map(|models| {
            models
                .iter()
                .filter_map(|m| {
                    let name = m.get("name").or_else(|| m.get("model"))?.as_str()?;
                    let remote = m.get("remote_host").and_then(|h| h.as_str()).is_some();
                    Some((name.to_string(), remote || name.ends_with("-cloud")))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// What `host` has installed, or `None` when nothing answered there. Never an error: a
/// missing Ollama is the ordinary case, not a fault.
pub fn discover(host: &str) -> Option<Models> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(TIMEOUT))
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .build()
        .into();
    let url = format!("{}/api/tags", host.trim_end_matches('/'));
    let mut resp = agent.get(&url).call().ok()?;
    if !(200..300).contains(&resp.status().as_u16()) {
        return None;
    }
    let body = resp.body_mut().read_to_string().ok()?;
    Some(classify(&names_in(&body)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_are_split_by_what_each_model_can_serve() {
        let body = r#"{"models":[
            {"name":"llama3.2:latest"},
            {"name":"ZimaBlueAI/whisper-large-v3:latest"},
            {"name":"nomic-embed-text:latest"},
            {"name":"qwen2.5:7b"}
        ]}"#;
        let m = classify(&names_in(body));
        assert_eq!(m.text, ["llama3.2:latest", "qwen2.5:7b"]);
        assert_eq!(
            m.unusable_speech,
            ["ZimaBlueAI/whisper-large-v3:latest"],
            "named so the wizard can say why, never offered"
        );
    }

    #[test]
    fn a_body_that_is_not_a_catalogue_yields_nothing() {
        assert!(names_in("not json").is_empty());
        assert!(names_in(r#"{"models":null}"#).is_empty());
        assert!(classify(&names_in("{}")).is_empty());
    }

    #[test]
    fn cloud_models_are_kept_apart_from_local_ones() {
        let body = r#"{"models":[
            {"name":"sendmeaiohyeah/whisper-large-v2:latest","model":"sendmeaiohyeah/whisper-large-v2:latest"},
            {"name":"gemma4:e2b","model":"gemma4:e2b"},
            {"name":"gemma4:31b-cloud","model":"gemma4:31b","remote_host":"https://ollama.com"},
            {"name":"kimi-k2:1t","model":"kimi-k2:1t","remote_host":"https://ollama.com"}
        ]}"#;
        let m = classify(&names_in(body));
        assert_eq!(
            m.unusable_speech,
            ["sendmeaiohyeah/whisper-large-v2:latest"]
        );
        assert_eq!(
            m.text,
            ["gemma4:e2b"],
            "only what runs here is \"installed\""
        );
        assert_eq!(
            m.cloud,
            ["gemma4:31b-cloud", "kimi-k2:1t"],
            "remote_host marks a hosted model, tag or no tag"
        );
    }

    #[test]
    fn the_openai_surface_is_the_host_plus_v1() {
        assert_eq!(
            base_url("http://127.0.0.1:11434"),
            "http://127.0.0.1:11434/v1"
        );
        assert_eq!(base_url("http://h:11434/"), "http://h:11434/v1");
    }
}
