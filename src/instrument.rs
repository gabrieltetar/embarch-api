//! Bench instruments: building a declaration from a tool's or a subcommand's
//! flat parameters, and the one model preset the field has measured
//! (decision 97). Core does the talking (`embarch-core` decision 104); the
//! declaration's shape is `embarch-topology` decision 49's.

use embarch_core_client::{Instrument, InstrumentProtocol, UsbPortId};

/// What a declaration is built from: every field optional except the name
/// and the port, so a preset can fill the rest and an explicit value wins.
#[derive(Debug, Default, Clone)]
pub struct DeclareArgs<'a> {
    pub name: &'a str,
    pub port: &'a str,
    pub interface: Option<u8>,
    pub preset: Option<&'a str>,
    pub baud: Option<u32>,
    pub write_terminator: Option<&'a str>,
    pub read_terminator: Option<&'a str>,
    pub min_interval_ms: Option<u32>,
    pub ack_ok: Option<&'a str>,
    pub ack_err: Option<&'a str>,
    /// The commands that switch the output off, one per line. `None` keeps
    /// what Core already stores under the name ([`carry_stored_safe_off`]);
    /// `Some(&[])` clears it.
    pub safe_off: Option<&'a [String]>,
}

/// The presets this crate knows, by name. Each is a set of line facts seen on
/// the unit or confirmed by its manual, never a guess.
pub const PRESETS: &[&str] = &["et54"];

/// One preset's facts: baud, write and read terminators, pacing, acks.
struct Preset {
    baud: u32,
    write: &'static str,
    read: &'static str,
    min_interval_ms: u32,
    ack_ok: &'static str,
    ack_err: &'static str,
}

/// An ET54-series electronic load behind its CH340 bridge, as measured in
/// the field (decision 97): 9600 8N1, LF to it (CR alone is ignored),
/// CR LF back, at least 200 ms between commands, and a set answers
/// `Rexecu success` or `Rexecu err`.
const ET54: Preset =
    Preset { baud: 9600, write: "\n", read: "\r\n", min_interval_ms: 200, ack_ok: "Rexecu success", ack_err: "Rexecu err" };

fn preset(name: &str) -> Result<Preset, String> {
    match name.to_ascii_lowercase().as_str() {
        "et54" => Ok(ET54),
        other => Err(format!("unknown preset `{other}`; known: {}", PRESETS.join(", "))),
    }
}

/// `lf`, `cr` or `crlf` to the bytes; anything else is refused by name, so a
/// literal backslash never reaches an instrument.
pub fn terminator(name: &str) -> Result<String, String> {
    match name.to_ascii_lowercase().as_str() {
        "lf" => Ok("\n".into()),
        "cr" => Ok("\r".into()),
        "crlf" => Ok("\r\n".into()),
        other => Err(format!("terminator `{other}` is not one of lf, cr, crlf")),
    }
}

/// The declaration `args` describe. A preset fills what is not given; with
/// no preset, `baud` is required and the terminators default to LF out and
/// CR LF back.
pub fn build(args: &DeclareArgs<'_>) -> Result<Instrument, String> {
    let mut port: UsbPortId = args.port.parse()?;
    port.interface = args.interface;
    let p = args.preset.map(preset).transpose()?;
    let baud = args
        .baud
        .or(p.as_ref().map(|p| p.baud))
        .ok_or_else(|| format!("instrument `{}` needs a baud, or a preset ({})", args.name, PRESETS.join(", ")))?;
    let write_terminator = match args.write_terminator {
        Some(t) => terminator(t)?,
        None => p.as_ref().map_or("\n", |p| p.write).to_string(),
    };
    let read_terminator = match args.read_terminator {
        Some(t) => terminator(t)?,
        None => p.as_ref().map_or("\r\n", |p| p.read).to_string(),
    };
    let inst = Instrument {
        name: args.name.to_string(),
        protocol: InstrumentProtocol::Scpi,
        port,
        baud,
        write_terminator,
        read_terminator,
        min_interval_ms: args.min_interval_ms.or(p.as_ref().map(|p| p.min_interval_ms)).unwrap_or(0),
        ack_ok: args.ack_ok.map(str::to_string).or(p.as_ref().map(|p| p.ack_ok.to_string())),
        ack_err: args.ack_err.map(str::to_string).or(p.as_ref().map(|p| p.ack_err.to_string())),
        safe_off: args.safe_off.map(<[String]>::to_vec).unwrap_or_default(),
    };
    inst.check()?;
    Ok(inst)
}

/// `PUT /instruments` replaces an instrument by name, so a declaration that
/// leaves `safe_off` out would drop one declared by another path (decision
/// 97). When the caller gave none (`args.safe_off` is `None`), copy the
/// stored instrument's into `inst`; an explicit list, empty included, is
/// sent as given. A stored value that cannot be read is an error, never a
/// silent drop.
pub async fn carry_stored_safe_off(core: &embarch_core_client::CoreClient, inst: &mut Instrument, args: &DeclareArgs<'_>) -> Result<(), String> {
    if args.safe_off.is_some() {
        return Ok(());
    }
    let stored = core
        .instruments()
        .await
        .map_err(|e| format!("could not read the stored `{}` to carry its safe_off over the re-declare (pass safe_off, empty to clear): {e:#}", inst.name))?;
    if let Some(old) = stored.into_iter().find(|i| i.name == inst.name) {
        inst.safe_off = old.safe_off;
    }
    Ok(())
}

/// One line describing a declaration, for the CLI's human output.
pub fn describe(inst: &Instrument) -> String {
    let t = |s: &str| s.replace('\r', "CR").replace('\n', "LF");
    format!(
        "{} on {} at {} baud, {} out / {} back, {} ms between commands, {}",
        inst.name,
        inst.port,
        inst.baud,
        t(&inst.write_terminator),
        t(&inst.read_terminator),
        inst.min_interval_ms,
        match (&inst.ack_ok, &inst.ack_err) {
            (Some(ok), Some(err)) => format!("a set acknowledged by '{ok}' (refused: '{err}')"),
            (Some(ok), None) => format!("a set acknowledged by '{ok}'"),
            _ => "a set not acknowledged".to_string(),
        }
    ) + &match inst.safe_off.len() {
        0 => ", no safe-off".to_string(),
        n => format!(", safe-off of {n} command{}", if n == 1 { "" } else { "s" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args<'a>(name: &'a str, port: &'a str) -> DeclareArgs<'a> {
        DeclareArgs { name, port, ..Default::default() }
    }

    #[test]
    fn the_et54_preset_fills_every_line_fact_and_an_explicit_value_wins() {
        let i = build(&DeclareArgs { preset: Some("ET54"), ..args("eload", "1a86:7523") }).unwrap();
        assert_eq!((i.baud, i.write_terminator.as_str(), i.read_terminator.as_str(), i.min_interval_ms), (9600, "\n", "\r\n", 200));
        assert_eq!((i.ack_ok.as_deref(), i.ack_err.as_deref()), (Some("Rexecu success"), Some("Rexecu err")));
        let i = build(&DeclareArgs { preset: Some("et54"), baud: Some(115_200), min_interval_ms: Some(300), ..args("eload", "1a86:7523") }).unwrap();
        assert_eq!((i.baud, i.min_interval_ms), (115_200, 300));
    }

    #[test]
    fn without_a_preset_baud_is_required_and_terminators_are_named() {
        assert!(build(&args("psu", "0483:5740")).unwrap_err().contains("needs a baud"));
        let i = build(&DeclareArgs { baud: Some(9600), read_terminator: Some("lf"), interface: Some(2), ..args("psu", "0483:5740") }).unwrap();
        assert_eq!((i.write_terminator.as_str(), i.read_terminator.as_str(), i.port.interface), ("\n", "\n", Some(2)));
        assert_eq!((i.ack_ok, i.ack_err), (None, None));
        assert!(build(&DeclareArgs { baud: Some(9600), write_terminator: Some("\\n"), ..args("psu", "0483:5740") }).is_err());
        assert!(build(&DeclareArgs { preset: Some("hp34401"), ..args("dmm", "0483:5740") }).unwrap_err().contains("known: et54"));
        assert!(build(&DeclareArgs { baud: Some(9600), ..args("bad name", "0483:5740") }).is_err());
        assert!(build(&DeclareArgs { baud: Some(9600), ..args("psu", "zz") }).is_err());
    }

    #[test]
    fn safe_off_is_carried_as_given_and_empty_when_omitted() {
        let lines = vec!["INP OFF".to_string(), "CURR 0".to_string()];
        let i = build(&DeclareArgs { baud: Some(9600), safe_off: Some(&lines), ..args("eload", "1a86:7523") }).unwrap();
        assert_eq!(i.safe_off, lines);
        assert!(describe(&i).ends_with("safe-off of 2 commands"));
        let i = build(&DeclareArgs { baud: Some(9600), ..args("eload", "1a86:7523") }).unwrap();
        assert!(i.safe_off.is_empty() && describe(&i).ends_with("no safe-off"));
        let bad = vec!["a\nb".to_string()];
        assert!(build(&DeclareArgs { baud: Some(9600), safe_off: Some(&bad), ..args("eload", "1a86:7523") }).unwrap_err().contains("safe_off"));
    }

    #[test]
    fn a_declaration_reads_back_in_words() {
        let i = build(&DeclareArgs { preset: Some("et54"), ..args("eload", "1a86:7523") }).unwrap();
        let d = describe(&i);
        assert!(d.contains("1a86:7523 at 9600 baud, LF out / CRLF back, 200 ms"), "{d}");
    }
}
