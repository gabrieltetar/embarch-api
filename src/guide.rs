//! `embarch-api guide hil [mode]` — serves the HIL guide embedded at build
//! (`tasks/api/163`, decision 108). Host-side only: no config, no Core, no
//! `embarch-doc` on disk, so `main` dispatches it before any of them.
//!
//! The text is `guide/hil-guide.md`, a pinned copy of `embarch-doc`'s
//! `suite/hil-guide.md` compiled in with `include_str!`. Sections are the
//! file's `## ` headings: "Choosing a mode" is what `guide hil` prints, and
//! `Live`, `Study` and `Test` are the modes.

use clap::Subcommand;
use serde_json::json;

pub const HIL_GUIDE: &str = include_str!("../guide/hil-guide.md");

const MODE_SECTION: &str = "Choosing a mode";
pub const MODES: [&str; 3] = ["live", "study", "test"];

#[derive(Subcommand, Debug)]
pub enum GuideAction {
    /// Print the hardware-in-the-loop guide: with no mode, the "which mode"
    /// part; with `live`, `study` or `test`, that mode's section.
    Hil {
        /// live, study or test.
        mode: Option<String>,
    },
}

/// The body of the `## <heading>` section of `text` (heading line included),
/// up to the next `## ` heading.
fn section<'a>(text: &'a str, heading: &str) -> Option<&'a str> {
    let marker = format!("## {heading}");
    let mut start = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let is_h2 = line.starts_with("## ");
        if let Some(s) = start {
            if is_h2 {
                return Some(text[s..offset].trim_end());
            }
        } else if is_h2 && line.trim_end() == marker {
            start = Some(offset);
        }
        offset += line.len();
    }
    start.map(|s| text[s..].trim_end())
}

fn heading_of(mode: &str) -> String {
    let mut c = mode.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

fn render(mode: Option<&str>) -> Result<String, String> {
    let heading = match mode {
        None => MODE_SECTION.to_string(),
        Some(m) if MODES.contains(&m) => heading_of(m),
        Some(m) => {
            return Err(format!("unknown mode {m:?}; the modes are {}", MODES.join(", ")));
        }
    };
    section(HIL_GUIDE, &heading)
        .map(str::to_string)
        .ok_or_else(|| format!("the embedded guide has no `{heading}` section"))
}

pub fn run(action: GuideAction, json: bool) -> i32 {
    let GuideAction::Hil { mode } = action;
    match render(mode.as_deref()) {
        Ok(text) => {
            if json {
                println!(
                    "{}",
                    embarch_api::json_out::pretty(json!({ "success": true, "mode": mode, "text": text }))
                );
            } else {
                println!("{text}");
            }
            0
        }
        Err(message) => {
            if json {
                println!(
                    "{}",
                    embarch_api::json_out::pretty(json!({ "success": false, "error": message }))
                );
            } else {
                eprintln!("{message}");
            }
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_has_a_section_and_sections_do_not_overlap() {
        for m in MODES {
            let s = render(Some(m)).unwrap();
            assert!(s.starts_with(&format!("## {}", heading_of(m))));
            assert_eq!(s.matches("\n## ").count(), 0);
        }
        assert!(render(None).unwrap().starts_with("## Choosing a mode"));
    }

    #[test]
    fn unknown_mode_lists_the_modes() {
        let e = render(Some("bogus")).unwrap_err();
        assert!(e.contains("live, study, test"), "{e}");
    }
}
