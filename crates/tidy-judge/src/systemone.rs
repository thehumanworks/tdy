//! SystemOne-compatible decision APIs: TypeSafe Jev (`/v1/systemone`),
//! Cloudflare Clef / Clef-flash (Workers AI REST or local), DigitalOcean.
//!
//! Request: `{model, state, questions: {name: {type: noul|choice|score,
//! instructions, criteria}}, images?: [data-url]}` (images are a Clef
//! extension). Response: `{answers: {name: …}, usage}`, possibly wrapped in
//! Workers AI's `{result: …, success}` envelope.

use crate::http::Http;
use crate::ir::{Answer, Caps, QKind, Request, Response};
use crate::{Judge, JudgeError};
use serde_json::{Map, Value, json};
use std::time::{Duration, Instant};

pub const TYPESAFE_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";

pub struct SystemOne {
    http: Http,
    model: String,
    name: String,
    caps: Caps,
}

impl SystemOne {
    pub fn new(
        name: &str,
        model: &str,
        endpoint: Option<&str>,
        api_key: &str,
        timeout: Duration,
        concurrency: usize,
        caps: Caps,
    ) -> Result<Self, JudgeError> {
        let url = endpoint.unwrap_or(TYPESAFE_ENDPOINT).to_string();
        let local = url.starts_with("http://localhost") || url.starts_with("http://127.0.0.1");
        let mut headers = Vec::new();
        if !api_key.is_empty() {
            crate::http::validate_url(&url)?;
            headers.push(("Authorization".into(), format!("Bearer {api_key}")));
        } else if !local {
            return Err(JudgeError::Config(format!("judge `{name}` has no API key")));
        }
        Ok(SystemOne { http: Http::new(url, headers, timeout, concurrency), model: model.into(), name: name.into(), caps })
    }
}

pub fn body(model: &str, req: &Request, images: bool) -> Value {
    let mut qs = Map::new();
    for q in &req.questions {
        let v = match &q.kind {
            QKind::Predicate => json!({"type": "noul", "instructions": q.instructions}),
            QKind::Choice { options } => {
                let mut c = Map::new();
                for (val, desc) in options {
                    c.insert(val.clone(), Value::String(desc.clone()));
                }
                json!({"type": "choice", "instructions": q.instructions, "criteria": c})
            }
            QKind::Score { levels } => json!({
                "type": "score", "instructions": q.instructions,
                "criteria": levels.iter().map(|(l, d)| if d.is_empty() { json!(l) } else { json!({"label": l, "description": d}) }).collect::<Vec<_>>()
            }),
        };
        qs.insert(q.name.clone(), v);
    }
    let mut state = req.text.clone();
    let mut b = json!({"model": model, "questions": qs});
    if images && !req.images.is_empty() {
        b["images"] = Value::Array(req.images.iter().map(|i| Value::String(i.data_url())).collect());
        let captions: Vec<String> =
            req.images.iter().enumerate().filter_map(|(n, i)| i.caption.as_ref().map(|c| format!("image {}: {c}", n + 1))).collect();
        if !captions.is_empty() {
            state.push_str("\n\nIMAGES\n");
            state.push_str(&captions.join("\n"));
        }
    }
    b["state"] = Value::String(state);
    b
}

pub fn parse(v: &Value, req: &Request) -> Result<Response, JudgeError> {
    let v = match v.get("result") {
        Some(r) if r.get("answers").is_some() => r,
        _ => v,
    };
    let answers =
        v.get("answers").and_then(Value::as_object).ok_or_else(|| JudgeError::Protocol("response has no `answers` object".into()))?;
    let mut out = Response::default();
    for q in &req.questions {
        let Some(a) = answers.get(&q.name) else {
            out.answers.insert(q.name.clone(), Answer::Refusal);
            continue;
        };
        let p = |x: &Value| x.as_f64().map(|f| if f > 1.0 { f / 100.0 } else { f });
        let ans = match a.get("type").and_then(Value::as_str).unwrap_or("") {
            "noul" | "predicate" | "bool" => match a.get("noul").or_else(|| a.get("probability")).and_then(p) {
                Some(x) => Answer::Predicate { p: x },
                None => Answer::Refusal,
            },
            "choice" => {
                let probs: Vec<(String, f64)> = match a.get("probabilities") {
                    Some(Value::Object(m)) => m.iter().filter_map(|(k, v)| Some((k.clone(), p(v)?))).collect(),
                    _ => vec![],
                };
                Answer::Choice {
                    choice: a.get("choice").and_then(Value::as_str).unwrap_or_default().to_string(),
                    probs,
                    confidence: a.get("confidence").and_then(p),
                }
            }
            "score" => {
                let mut probs: Vec<(usize, f64)> = match a.get("probabilities") {
                    Some(Value::Object(m)) => m.iter().filter_map(|(k, v)| Some((k.parse().ok()?, p(v)?))).collect(),
                    Some(Value::Array(xs)) => xs.iter().enumerate().filter_map(|(i, v)| Some((i, p(v)?))).collect(),
                    _ => vec![],
                };
                probs.sort_by_key(|x| x.0);
                Answer::Score {
                    score: a.get("score").and_then(Value::as_f64).unwrap_or(0.0),
                    probs: probs.into_iter().map(|x| x.1).collect(),
                    confidence: a.get("confidence").and_then(p),
                }
            }
            _ => Answer::Refusal,
        };
        out.answers.insert(q.name.clone(), ans);
    }
    out.input_tokens = v.pointer("/usage/input_tokens").or_else(|| v.pointer("/usage/prompt_tokens")).and_then(Value::as_u64).unwrap_or(0);
    Ok(out)
}

impl Judge for SystemOne {
    fn id(&self) -> String {
        format!("{}:systemone:{}", self.name, self.model)
    }
    fn caps(&self) -> Caps {
        self.caps
    }
    fn decide(&self, req: &Request) -> Result<Response, JudgeError> {
        let t = Instant::now();
        let v = self.http.post(&body(&self.model, req, self.caps.images))?;
        let mut r = parse(&v, req)?;
        self.http.usage.input_tokens.fetch_add(r.input_tokens, std::sync::atomic::Ordering::Relaxed);
        r.latency_ms = t.elapsed().as_millis() as u64;
        r.judge = self.id();
        Ok(r)
    }
    fn usage(&self) -> Option<&crate::http::Usage> {
        Some(&self.http.usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Question;
    #[test]
    fn body_and_parse_match_docs() {
        let req = Request {
            text: "late delivery, great product".into(),
            images: vec![],
            questions: vec![
                Question::predicate("followup", "Needs follow-up?"),
                Question::choice("sentiment", "Overall?", vec![("positive".into(), "happy".into()), ("mixed".into(), "both".into())]),
                Question::score(
                    "sat",
                    "Satisfaction",
                    vec![("low".into(), "".into()), ("medium".into(), "".into()), ("high".into(), "".into())],
                ),
            ],
        };
        let b = body("jev-latest", &req, false);
        assert_eq!(b["questions"]["followup"]["type"], "noul");
        assert_eq!(b["questions"]["sentiment"]["criteria"]["mixed"], "both");
        assert_eq!(b["questions"]["sat"]["criteria"][2], "high");
        let resp = json!({"result": {"answers": {
            "followup": {"type": "noul", "noul": 0.68},
            "sentiment": {"type": "choice", "choice": "mixed", "probabilities": {"mixed": 1, "positive": 0}, "confidence": 1},
            "sat": {"type": "score", "score": 1.32, "legend": {"0": "low"}, "probabilities": {"0": 0, "1": 0.68, "2": 0.32}, "confidence": 0.52}
        }, "usage": {"input_tokens": 349}}, "success": true});
        let r = parse(&resp, &req).unwrap();
        assert_eq!(r.answers["followup"].p(), Some(0.68));
        assert!(matches!(&r.answers["sentiment"], Answer::Choice { choice, .. } if choice == "mixed"));
        assert!(matches!(&r.answers["sat"], Answer::Score { probs, .. } if probs == &vec![0.0, 0.68, 0.32]));
        assert_eq!(r.input_tokens, 349);
    }
}
