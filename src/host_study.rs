//! Host-run studies and their operator prompts (decision 99): reading a
//! `HostStudy` file and checking a relayed answer, shared by the MCP tools and
//! the subcommands so the two cannot disagree. Core runs the study and owns
//! the prompt (`embarch-core` decisions 95, 105).

use std::path::Path;

use anyhow::{bail, Context, Result};
use embarch_core_client::StudyStatusResponse;
use embarch_study_designer::HostStudy;

/// A `HostStudy` from a JSON file, checked by its own rules before Core sees
/// it, so a malformed study is refused here with the designer's own words.
pub fn read_host_study(path: &Path) -> Result<HostStudy> {
    let bytes = std::fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    let study: HostStudy =
        serde_json::from_slice(&bytes).with_context(|| format!("{} is not a HostStudy (JSON)", path.display()))?;
    if let Err(e) = study.validate() {
        bail!("{} is not a valid host study: {e:?}", path.display());
    }
    Ok(study)
}

/// `study_status`'s and `study-status`'s body, so the two cannot disagree: Core's own fields
/// verbatim, plus a host study's record once it has one (`host_result`) and the `Prompt` waiting
/// for a person (`pending_prompt`, with `bench_config` when the step names one). Both are null
/// when absent, which for a bench study is always.
pub fn study_status_json(resp: &StudyStatusResponse) -> serde_json::Value {
    serde_json::json!({
        "status": resp.status,
        "current_step": resp.current_step,
        "total_steps": resp.total_steps,
        "result": resp.result,
        "reason": resp.reason,
        "host_result": resp.host_result,
        "pending_prompt": resp.pending_prompt,
    })
}

/// The two answers a person can give, as Core spells them.
pub fn parse_answer(answer: &str) -> Result<&'static str> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "confirmed" | "confirm" => Ok("confirmed"),
        "cancelled" | "cancel" | "canceled" => Ok("cancelled"),
        other => bail!("answer must be confirmed or cancelled, not '{other}'"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_two_answers_are_accepted() {
        assert_eq!(parse_answer("confirmed").unwrap(), "confirmed");
        assert_eq!(parse_answer(" Confirm ").unwrap(), "confirmed");
        assert_eq!(parse_answer("canceled").unwrap(), "cancelled");
        assert!(parse_answer("yes").is_err());
        assert!(parse_answer("").is_err());
    }

    #[test]
    fn study_status_carries_a_host_record_and_a_waiting_prompt() {
        let body = serde_json::json!({
            "status": "running", "current_step": 1, "total_steps": 2, "result": null, "reason": null,
            "pending_prompt": { "prompt_id": 2, "list": "Steps", "step_index": 1, "step_name": "swap",
                "text": "move the analyzer in line", "bench_config": "analyzer in line",
                "opened_utc_ms": 1, "deadline_utc_ms": 9 },
        });
        let v = study_status_json(&embarch_core_client::study_status_from_json(body).unwrap());
        assert_eq!(v["pending_prompt"]["prompt_id"], 2);
        assert_eq!(v["pending_prompt"]["bench_config"], "analyzer in line");
        assert!(v["host_result"].is_null() && v["result"].is_null());

        let body = serde_json::json!({
            "status": "completed", "current_step": 1, "total_steps": 2, "reason": null,
            "result": { "kind": "host", "study_name": "pd", "completed": false, "stopped_reason": "step 'b' failed", "steps": [] },
        });
        let v = study_status_json(&embarch_core_client::study_status_from_json(body).unwrap());
        assert_eq!((v["host_result"]["study_name"].as_str(), v["host_result"]["completed"].as_bool()), (Some("pd"), Some(false)));
        assert!(v["pending_prompt"].is_null() && v["result"].is_null());
        assert_eq!(v["status"], "completed");
    }

    #[test]
    fn a_host_study_file_is_read_and_checked() {
        let dir = std::env::temp_dir().join(format!("embarch-api-host-study-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good.json");
        std::fs::write(
            &good,
            r#"{"name":"pd","steps":[{"name":"plug","action":{"Prompt":{"text":"plug in the pack"}},"timeout_ms":60000,"continue_on_fail":false,"delay_before_ms":0}],"teardown":[],"streams":[],"line_counts":[]}"#,
        )
        .unwrap();
        let s = read_host_study(&good).unwrap();
        assert_eq!(s.steps.len(), 1);
        // An agent authors a bench configuration in the same file: no tool parameter of its own.
        let named = dir.join("named.json");
        std::fs::write(
            &named,
            r#"{"name":"pd","steps":[{"name":"swap","action":{"Prompt":{"text":"move the analyzer in line","bench_config":"analyzer in line"}},"timeout_ms":60000}]}"#,
        )
        .unwrap();
        let s = read_host_study(&named).unwrap();
        assert!(matches!(
            &s.steps[0].action,
            embarch_study_designer::HostAction::Prompt { bench_config: Some(c), .. } if c == "analyzer in line"
        ));
        let blank = dir.join("blank.json");
        std::fs::write(
            &blank,
            r#"{"name":"pd","steps":[{"name":"swap","action":{"Prompt":{"text":"t","bench_config":" "}},"timeout_ms":60000}]}"#,
        )
        .unwrap();
        assert!(read_host_study(&blank).unwrap_err().to_string().contains("not a valid host study"));
        let empty = dir.join("empty.json");
        std::fs::write(&empty, r#"{"name":"pd","steps":[],"teardown":[],"streams":[],"line_counts":[]}"#).unwrap();
        assert!(read_host_study(&empty).unwrap_err().to_string().contains("not a valid host study"));
        assert!(read_host_study(&dir.join("missing.json")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
