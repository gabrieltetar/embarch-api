//! The identity acts (decision 143): what an attested Core refuses the agent's token.
//!
//! On an attested Core the ten acts below are the person's. Core answers the agent's token
//! `403`, and this module words the tool's answer to it: the act, the terminal command that
//! makes it with the person's token (`--as-person`), and the UI tab that makes the same act.
//! Nothing here sends the person's token; `cli.rs` does, on the CLI only.

/// One identity act, named as its MCP tool and its CLI subcommand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityAct {
    EnrollProbe,
    UnenrollProbe,
    SetRoleBoard,
    DeclareInstrument,
    RemoveInstrument,
    DeclareSignal,
    RemoveSignal,
    DeclareBootloadPorts,
    ClearBootloadPorts,
    DevBenchLink,
}

/// Every identity act, for the test that holds the descriptions to the rule.
pub const ALL: [IdentityAct; 10] = [
    IdentityAct::EnrollProbe,
    IdentityAct::UnenrollProbe,
    IdentityAct::SetRoleBoard,
    IdentityAct::DeclareInstrument,
    IdentityAct::RemoveInstrument,
    IdentityAct::DeclareSignal,
    IdentityAct::RemoveSignal,
    IdentityAct::DeclareBootloadPorts,
    IdentityAct::ClearBootloadPorts,
    IdentityAct::DevBenchLink,
];

/// The UI surface that makes the same act.
const UI_SURFACE: &str = "embarch-ui's Topology tab";

impl IdentityAct {
    /// The MCP tool's name.
    pub fn tool(self) -> &'static str {
        match self {
            IdentityAct::EnrollProbe => "enroll_probe",
            IdentityAct::UnenrollProbe => "unenroll_probe",
            IdentityAct::SetRoleBoard => "set_role_board",
            IdentityAct::DeclareInstrument => "declare_instrument",
            IdentityAct::RemoveInstrument => "remove_instrument",
            IdentityAct::DeclareSignal => "declare_signal",
            IdentityAct::RemoveSignal => "remove_signal",
            IdentityAct::DeclareBootloadPorts => "declare_bootload_ports",
            IdentityAct::ClearBootloadPorts => "clear_bootload_ports",
            IdentityAct::DevBenchLink => "dev_bench_link",
        }
    }

    /// The CLI subcommand that makes the act.
    pub fn command(self) -> String {
        self.tool().replace('_', "-")
    }

    /// The act as a sentence fragment.
    fn what(self) -> &'static str {
        match self {
            IdentityAct::EnrollProbe => "enroll a probe",
            IdentityAct::UnenrollProbe => "retract an enrolled board",
            IdentityAct::SetRoleBoard => "declare the board type in a role",
            IdentityAct::DeclareInstrument => "declare an instrument",
            IdentityAct::RemoveInstrument => "remove an instrument",
            IdentityAct::DeclareSignal => "declare a signal",
            IdentityAct::RemoveSignal => "remove a signal",
            IdentityAct::DeclareBootloadPorts => "declare the bootload ports",
            IdentityAct::ClearBootloadPorts => "clear the bootload ports",
            IdentityAct::DevBenchLink => "declare dev-bench's link",
        }
    }

    /// The act for a CLI subcommand name, `None` for any other subcommand.
    pub fn from_command(name: &str) -> Option<IdentityAct> {
        ALL.into_iter().find(|a| a.command() == name)
    }

    /// What an identity tool adds to Core's refusal, `None` unless it is a person-scope `403`.
    pub fn refusal_note(self, err: &str) -> Option<String> {
        err.contains("403").then(|| {
            format!(
                " | Core is attested, so this act is the person's: it refused your token. You would have made it to {}. \
                 The person can make it in their own terminal with `embarch-api {} --as-person ...` (the same arguments, \
                 then a typed `yes`) or in {UI_SURFACE}. Do not retry, and do not ask for their token.",
                self.what(),
                self.command()
            )
        })
    }

    /// The tool's error text: `message`, with the refusal note when Core refused the token.
    pub fn error_text(self, message: String) -> String {
        match self.refusal_note(&message) {
            Some(note) => format!("{message}{note}"),
            None => message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_403_names_the_command_and_the_ui_tab_for_every_act() {
        for act in ALL {
            let text = act.error_text(format!("{} failed: embarch-core returned 403 Forbidden: person scope", act.tool()));
            assert!(text.contains(&format!("embarch-api {} --as-person", act.command())), "{text}");
            assert!(text.contains("embarch-ui's Topology tab"), "{text}");
            assert!(text.contains(act.what()), "{text}");
        }
    }

    #[test]
    fn another_refusal_is_left_as_core_said_it() {
        let m = "enroll_probe failed: embarch-core returned 409 Conflict: busy".to_string();
        assert_eq!(IdentityAct::EnrollProbe.error_text(m.clone()), m);
    }

    #[test]
    fn a_command_name_finds_its_act() {
        assert_eq!(IdentityAct::from_command("declare-bootload-ports"), Some(IdentityAct::DeclareBootloadPorts));
        assert_eq!(IdentityAct::from_command("person"), None);
    }
}
