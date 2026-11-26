//! The client's prompt types and a Test run's prompt (`embarch-core` decision 138).

use embarch_core_client::{PendingPrompt, PromptAnswerRequest, StudyEvent};
use serde_json::json;

fn host_prompt() -> serde_json::Value {
    json!({
        "prompt_id": 2, "list": "Steps", "step_index": 2, "step_name": "plug",
        "text": "plug in the pack", "opened_utc_ms": 1000, "deadline_utc_ms": 61000,
    })
}

#[test]
fn a_host_studys_prompt_and_answer_are_byte_for_byte_what_they_were() {
    let p: PendingPrompt = serde_json::from_value(host_prompt()).unwrap();
    assert_eq!((p.kind, p.observation.clone(), p.switched_on.len()), (None, None, 0));
    assert_eq!(serde_json::to_value(&p).unwrap(), host_prompt());
    let a = PromptAnswerRequest { prompt_id: 2, answer: "confirmed".into(), answered_by: "ui".into(), run_fields: vec![], value: None };
    assert_eq!(serde_json::to_string(&a).unwrap(), r#"{"prompt_id":2,"answer":"confirmed","answered_by":"ui"}"#);
    let a = PromptAnswerRequest { value: Some(json!("pass")), ..a };
    assert_eq!(serde_json::to_string(&a).unwrap(), r#"{"prompt_id":2,"answer":"confirmed","answered_by":"ui","value":"pass"}"#);
}

#[test]
fn a_test_runs_prompt_keeps_kind_observation_and_switched_on() {
    let mut v = host_prompt();
    v["kind"] = json!("observation");
    v["observation"] = json!({"name": "led", "answers": ["on", "off"]});
    v["switched_on"] = json!([{"target": "eload", "line": "OUTP ON", "step": "load on"}]);
    let p: PendingPrompt = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(p.switched_on[0].line, "OUTP ON");
    assert_eq!(serde_json::to_value(&p).unwrap(), v);
}

#[test]
fn the_pending_event_carries_them_and_round_trips_to_the_prompt() {
    let mut v = host_prompt();
    v["observation"] = json!({"name": "led", "answers": ["on", "off"]});
    v["switched_on"] = json!([{"target": "eload", "line": "OUTP ON", "step": "load on"}]);
    v["study_id"] = json!("s1");
    // Core flattens a Test run's prompt `kind` onto the `kind`-tagged event, so
    // there the wire has two `kind` keys and serde refuses it ("duplicate
    // field"): core's to fix, filed. The event without a prompt kind reads.
    let ev: StudyEvent = serde_json::from_value({
        v["kind"] = json!("PromptPending");
        v
    })
    .unwrap();
    let p = ev.as_pending_prompt().unwrap();
    assert_eq!(p.switched_on.len(), 1);
    assert!(p.observation.is_some());
    let line = r#"{"kind":"PromptPending","kind":"clearance","study_id":"s1","prompt_id":2,"list":"Steps","step_index":2,"step_name":"p","text":"t","opened_utc_ms":1,"deadline_utc_ms":2}"#;
    assert!(serde_json::from_str::<StudyEvent>(line).is_err());
}
