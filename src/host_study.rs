//! Host-run studies and their operator prompts (decision 99): reading a
//! `HostStudy` file and checking a relayed answer, shared by the MCP tools and
//! the subcommands so the two cannot disagree. Core runs the study and owns
//! the prompt (`embarch-core` decisions 95, 105).

use std::path::Path;

use anyhow::{bail, Context, Result};
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
        let empty = dir.join("empty.json");
        std::fs::write(&empty, r#"{"name":"pd","steps":[],"teardown":[],"streams":[],"line_counts":[]}"#).unwrap();
        assert!(read_host_study(&empty).unwrap_err().to_string().contains("not a valid host study"));
        assert!(read_host_study(&dir.join("missing.json")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
