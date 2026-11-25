//! `dev-bench-link` / `dev_bench_link`: the one implementation both front-ends
//! call (`tool_subcommand_parity`), over Core's `POST /dev-bench/link`.
//!
//! `control_port` is the USB identity of a bench whose control link is a CDC
//! ACM port rather than a SEGGER J-Link UART (`embarch-core` decision 124,
//! `embarch-dev-bench` decision 62). VID and PID are **hexadecimal**, with or
//! without `0x`, the spelling `lsusb`, `declare-bootload-ports` and Core's own
//! `VID:PID` parser use; a decimal `4660` would read as `0x4660`, so a value
//! that is not hex digits is refused rather than guessed at.

use embarch_core_client::{CoreClient, UsbPortId};

/// A control port's identity as a caller spells it.
#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
pub struct ControlPortArg {
    /// USB vendor ID, hexadecimal, with or without `0x` (e.g. `2fe3`).
    pub vid: String,
    /// USB product ID, hexadecimal, with or without `0x` (e.g. `0100`).
    pub pid: String,
    /// The device's own USB serial number, when more than one of this VID:PID is plugged in.
    #[serde(default)]
    pub serial: Option<String>,
    /// Which interface of a composite device is the control port.
    #[serde(default)]
    pub interface: Option<u8>,
}

/// Everything `POST /dev-bench/link` takes.
#[derive(Debug, Clone, Default)]
pub struct LinkArgs {
    pub serial: Option<String>,
    pub interface: Option<u8>,
    pub control_port: Option<ControlPortArg>,
    pub clear_control_port: bool,
}

fn hex_u16(what: &str, spelled: &str) -> Result<u16, String> {
    let digits = spelled.trim().trim_start_matches("0x").trim_start_matches("0X");
    u16::from_str_radix(digits, 16)
        .map_err(|_| format!("control_port {what} `{spelled}` is not a 16-bit hexadecimal number (e.g. 2fe3 or 0x2fe3)"))
}

impl ControlPortArg {
    /// Validates the spelling the way Core types it: 16-bit hex VID and PID,
    /// an empty serial meaning none.
    pub fn parse(&self) -> Result<UsbPortId, String> {
        Ok(UsbPortId {
            vid: hex_u16("vid", &self.vid)?,
            pid: hex_u16("pid", &self.pid)?,
            serial: self.serial.clone().filter(|s| !s.is_empty()),
            interface: self.interface,
        })
    }
}

/// Posts the link, or says why it will not: Core's own refusals (none of the
/// four given, `control_port` with `clear_control_port`) are checked here
/// rather than spent on a round trip. The `Ok` value is the success JSON both
/// front-ends report.
pub async fn apply(core: &CoreClient, args: &LinkArgs) -> Result<serde_json::Value, String> {
    let control = args.control_port.as_ref().map(ControlPortArg::parse).transpose()?;
    if control.is_some() && args.clear_control_port {
        return Err("control_port and clear_control_port contradict each other".to_string());
    }
    core.set_dev_bench_link(args.serial.as_deref(), args.interface, control.as_ref(), args.clear_control_port)
        .await
        .map_err(|e| format!("{e:#}"))?;
    Ok(serde_json::json!({
        "serial": args.serial,
        "interface": args.interface,
        "control_port": control,
        "clear_control_port": args.clear_control_port,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arg(vid: &str, pid: &str) -> ControlPortArg {
        ControlPortArg { vid: vid.into(), pid: pid.into(), serial: None, interface: None }
    }

    #[test]
    fn vid_and_pid_are_hex_with_or_without_a_prefix() {
        let id = arg("0x2FE3", "0100").parse().unwrap();
        assert_eq!((id.vid, id.pid), (0x2fe3, 0x0100));
    }

    #[test]
    fn a_value_that_is_not_16_bit_hex_is_refused() {
        assert!(arg("zz", "0100").parse().is_err());
        assert!(arg("2fe3", "").parse().is_err());
        assert!(arg("10000", "0100").parse().is_err());
    }

    #[test]
    fn an_empty_serial_means_none() {
        let a = ControlPortArg { serial: Some(String::new()), ..arg("2fe3", "0100") };
        assert_eq!(a.parse().unwrap().serial, None);
    }
}
