//! `embarch-api person ...`: the engineer's own acts from a terminal (decision 141).
//!
//! A confirmation, a Test run's go, an observation's answer and the two declarations of
//! `embarch-core` decision 147 are the **person's**, so this verb sends the person's token
//! and nothing else does. It is a CLI verb with no MCP tool: `tests/person_verb.rs` fails
//! if `src/tools.rs` names it, and `cli::run` hands it to [`run`] before anything that
//! holds the agent's client.
//!
//! Every subcommand prints what Core rendered (every string quoted, so text an agent
//! supplied cannot pass for Core's own words) and sends only after a typed `yes`.

use std::io::BufRead;

use embarch_core_client::{CoreClient, CoreConfig, TokenScope};
use serde_json::{json, Value};

/// The answer that sends; anything else, an empty line or a closed stdin sends nothing.
const YES: &str = "yes";
/// The surface Core records beside a confirmation or an answer (it records it and never trusts it).
const SURFACE: &str = "cli";

#[derive(clap::Subcommand, Debug)]
pub enum PersonAction {
    /// Show a confirmation card and, on a typed `yes`, confirm exactly what it showed.
    Confirm {
        /// The definition's hash (`confirmations pending` lists them).
        hash: String,
    },
    /// Show a prepared Test run's go card and, on a typed `yes`, give the go.
    Go { run_id: String },
    /// Answer the prompt a Test run waits on: an observation's value, else confirmed or cancelled.
    Answer {
        run_id: String,
        /// The id the waiting prompt carries (`test-status` shows it).
        prompt_id: u32,
        /// An observation's value: one of its declared answers, or a number.
        value: Option<String>,
        /// Answer `cancelled` instead of `confirmed`.
        #[arg(long, conflicts_with = "value")]
        cancel: bool,
    },
    /// Declare a unit's serial and board revision.
    Unit {
        key: String,
        #[arg(long)]
        serial: String,
        #[arg(long)]
        revision: String,
    },
    /// Declare your name.
    Name { name: String },
}

/// What one act came to.
struct Outcome {
    /// Core accepted it.
    sent: bool,
    value: Value,
    human: String,
}

impl Outcome {
    fn failed(message: String) -> Outcome {
        Outcome { sent: false, value: json!({ "success": false, "sent": false, "error": message }), human: message }
    }
    fn declined(what: &str) -> Outcome {
        let human = format!("not sent: no typed `{YES}`; {what} was left as it was");
        Outcome { sent: false, value: json!({ "success": false, "sent": false, "error": human }), human }
    }
    fn done(what: &str, response: Value) -> Outcome {
        Outcome { sent: true, value: json!({ "success": true, "sent": true, "response": response }), human: format!("sent: {what}") }
    }
}

/// Runs one act with the person's token, reading the `yes` from stdin; the process exit code.
pub async fn run(action: PersonAction, json: bool, core: &CoreConfig) -> i32 {
    let outcome = match CoreClient::new_with_scope(core, TokenScope::Person) {
        Ok(client) => act(&client, action, &mut std::io::stdin().lock()).await,
        Err(e) => Outcome::failed(format!("person: {e:#}")),
    };
    let code = if outcome.sent { 0 } else { 1 };
    if json {
        println!("{}", embarch_api::json_out::pretty(outcome.value));
    } else if outcome.sent {
        println!("{}", outcome.human);
    } else {
        eprintln!("{}", outcome.human);
    }
    code
}

async fn act(client: &CoreClient, action: PersonAction, input: &mut dyn BufRead) -> Outcome {
    match action {
        PersonAction::Confirm { hash } => confirm(client, &hash, input).await,
        PersonAction::Go { run_id } => go(client, &run_id, input).await,
        PersonAction::Answer { run_id, prompt_id, value, cancel } => answer(client, &run_id, prompt_id, value, cancel, input).await,
        PersonAction::Unit { key, serial, revision } => {
            let shown = json!({ "unit": key, "serial": serial, "board_revision": revision });
            if !ask(&shown, "declare this unit", input) {
                return Outcome::declined("the unit");
            }
            result(client.declare_unit(&key, &serial, &revision).await, "the unit is declared")
        }
        PersonAction::Name { name } => {
            if !ask(&json!({ "name": name }), "declare this name", input) {
                return Outcome::declined("the name");
            }
            result(client.declare_person(&name).await, "the name is declared")
        }
    }
}

fn result(r: anyhow::Result<Value>, what: &str) -> Outcome {
    match r {
        Ok(v) => Outcome::done(what, v),
        Err(e) => Outcome::failed(format!("{e:#}")),
    }
}

async fn confirm(client: &CoreClient, hash: &str, input: &mut dyn BufRead) -> Outcome {
    let card = match client.confirmation_card(hash).await {
        Ok(c) => c,
        Err(e) => return Outcome::failed(format!("{e:#}")),
    };
    let Some(nonce) = card["nonce"].as_str() else {
        return Outcome::failed(format!("nothing to confirm: Core issued no nonce for {hash} (confirmation: {})", card["confirmation"]));
    };
    if !ask(&card, &format!("confirm {hash}"), input) {
        return Outcome::declined("the confirmation");
    }
    result(client.confirm(hash, nonce, Some(SURFACE)).await, "the confirmation is made")
}

async fn go(client: &CoreClient, run_id: &str, input: &mut dyn BufRead) -> Outcome {
    let card = match client.test_run_card(run_id).await {
        Ok(c) => c,
        Err(e) => return Outcome::failed(format!("{e:#}")),
    };
    let (Some(nonce), Some(run_hash)) = (card["nonce"].as_str(), card["run_hash"].as_str()) else {
        return Outcome::failed(format!("no go to give: Core issued no nonce or run hash for {run_id}"));
    };
    if !ask(&card, &format!("give the go for {run_id}"), input) {
        return Outcome::declined("the run");
    }
    result(client.test_run_go(run_id, run_hash, nonce).await, "the go is given")
}

async fn answer(client: &CoreClient, run_id: &str, prompt_id: u32, value: Option<String>, cancel: bool, input: &mut dyn BufRead) -> Outcome {
    let status = match client.test_run_status(run_id).await {
        Ok(s) => s,
        Err(e) => return Outcome::failed(format!("{e:#}")),
    };
    let waiting = &status["waiting_prompt"];
    if waiting.is_null() {
        return Outcome::failed(format!("run {run_id} has no prompt waiting"));
    }
    if waiting["prompt_id"].as_u64() != Some(u64::from(prompt_id)) {
        return Outcome::failed(format!("run {run_id} is waiting on prompt {}, not {prompt_id}; nothing sent", waiting["prompt_id"]));
    }
    let value = value.map(|v| number_or_text(&v));
    let answer = if cancel { "cancelled" } else { "confirmed" };
    let shown = json!({ "run_id": run_id, "waiting_prompt": waiting, "your_answer": answer, "your_value": value });
    if !ask(&shown, &format!("send `{answer}` for prompt {prompt_id}"), input) {
        return Outcome::declined("the prompt");
    }
    let req = embarch_core_client::PromptAnswerRequest { prompt_id, answer: answer.to_string(), answered_by: SURFACE.to_string(), run_fields: vec![], value };
    match client.answer_prompt(run_id, &req).await {
        Ok(r) => Outcome::done("the prompt is answered", json!(r)),
        Err(e) => Outcome::failed(format!("{e:#}")),
    }
}

/// An observation takes a number or one of its declared answers.
fn number_or_text(v: &str) -> Value {
    match v.parse::<f64>().ok().filter(|n| n.is_finite()) {
        Some(n) => json!(n),
        None => json!(v),
    }
}

/// Prints `card` (to stderr, so `--json` keeps stdout) and reads one line; true only for `yes`.
pub fn ask(card: &Value, question: &str, input: &mut dyn BufRead) -> bool {
    eprintln!("{}", render(card));
    eprint!("{question}? Type {YES} to send: ");
    let mut line = String::new();
    input.read_line(&mut line).is_ok() && line.trim() == YES
}

/// Core's card as indented lines with **every** string quoted. The nonce is Core's to
/// carry, not the person's to read, so it is left out.
pub fn render(card: &Value) -> String {
    let mut out = String::new();
    lines(card, 0, &mut out);
    out
}

fn lines(v: &Value, depth: usize, out: &mut String) {
    let pad = "  ".repeat(depth);
    match v {
        Value::Object(m) => {
            for (k, x) in m.iter().filter(|(k, _)| !matches!(k.as_str(), "nonce" | "expires_utc_ms")) {
                match inline(x) {
                    Some(s) => out.push_str(&format!("{pad}{k}: {s}\n")),
                    None => {
                        out.push_str(&format!("{pad}{k}:\n"));
                        lines(x, depth + 1, out);
                    }
                }
            }
        }
        Value::Array(a) => {
            for x in a {
                match inline(x) {
                    Some(s) => out.push_str(&format!("{pad}- {s}\n")),
                    None => {
                        out.push_str(&format!("{pad}-\n"));
                        lines(x, depth + 1, out);
                    }
                }
            }
        }
        scalar => out.push_str(&format!("{pad}{}\n", inline(scalar).unwrap_or_default())),
    }
}

/// A leaf as one token (a string quoted, `{"agent_text": s}` as that string quoted), or
/// `None` for a container that needs its own lines.
fn inline(v: &Value) -> Option<String> {
    match v {
        Value::Object(m) if m.len() == 1 && m.contains_key("agent_text") => inline(&m["agent_text"]),
        Value::Object(m) if m.is_empty() => Some("{}".into()),
        Value::Array(a) if a.is_empty() => Some("[]".into()),
        Value::Object(_) | Value::Array(_) => None,
        Value::String(s) => Some(format!("{s:?}")),
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_string_is_quoted_and_the_nonce_is_not_shown() {
        let card = json!({ "nonce": "secret", "card": { "name": { "agent_text": "Plug\nIN" }, "kind": "setup", "n": 3, "list": [{ "agent_text": "a" }] } });
        let text = render(&card);
        assert!(text.contains("name: \"Plug\\nIN\""), "{text}");
        assert!(text.contains("kind: \"setup\""), "{text}");
        assert!(text.contains("n: 3"), "{text}");
        assert!(text.contains("- \"a\""), "{text}");
        assert!(!text.contains("secret"), "{text}");
    }

    #[test]
    fn a_value_is_a_number_when_it_reads_as_one() {
        assert_eq!(number_or_text("5.2"), json!(5.2));
        assert_eq!(number_or_text("green"), json!("green"));
        assert_eq!(number_or_text("NaN"), json!("NaN"));
    }
}
