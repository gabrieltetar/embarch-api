use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use embarch_study_designer::{StreamEncoding, Study, StudyResult};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OnceCell;

use crate::CoreConfig;
use embarch_topology::software::{ProbeOutcome, TopologyClass};

/// Where Core's address comes from.
enum Address {
    /// Declared in config, used as-is.
    Declared(String),
    /// `base_url = "auto"` — discovered by probing (config.rs, decision
    /// 11). Deliberately not resolved at construction time: the startup
    /// connectivity check is MCP-mode-only and `list_projects` is meant to
    /// work with Core down, both of which eager resolution would break.
    Auto {
        host: Option<String>,
        port: u16,
    },
}

#[derive(Clone)]
pub struct CoreClient {
    address: Arc<Address>,
    /// Resolution happens at most once per process. Not persisted anywhere:
    /// the next invocation re-resolves, which is exactly what makes a changed
    /// WSL2 gateway IP a non-event. Carries the winning `TopologyClass`
    /// alongside the address — `flash`'s only consumer (§9 of the design
    /// doc, the 2026-08-18 Session-0/UNC finding): a `WslHost`/`Remote` Core
    /// can't be assumed to share a filesystem with this process, so `flash`
    /// uploads bytes instead of sending a path for those classes.
    resolved: Arc<OnceCell<(String, TopologyClass)>>,
    token: String,
    client: reqwest::Client,
    status_timeout: Duration,
    reset_timeout: Duration,
    flash_timeout: Duration,
    serial_timeout: Duration,
    study_timeout: Duration,
}

// `Serialize`/`Clone` added 2026-08-24 (`ProbeInfo`, `StatusResponse`,
// `EnrolledBoardResponse`, `AlertResponse`, `DevBenchPortResponse`) —
// `embarch-ui`'s Dashboard/Topology tabs re-serialize what they deserialize
// from Core, to hand it back to the browser as JSON/SSE payloads
// (`embarch-ui/milestone-1.md` §4.4). `embarch-api` itself never needed
// either derive, but adding them is behavior-neutral for every existing
// deserialize-only caller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeInfo {
    pub identifier: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub serial_number: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub status: String,
    pub probes: Vec<ProbeInfo>,
    /// The **host type** schema version Core was built against
    /// (`embarch-study-designer` decision 12 and its
    /// 2026-08-25 amendment) — the number that guards this hop, which
    /// carries `Study`/`StudyResult` whole rather than the dev-bench subset.
    ///
    /// `Option` only so a Core predating the field still *parses*, which is
    /// what lets [`CoreClient::post_study`] report the drift by name instead
    /// of failing as an opaque JSON decode error. `None` is not treated as
    /// "compatible" — see that method.
    #[serde(default)]
    pub study_designer_schema_version: Option<u32>,
}

/// `embarch-api` and Core disagree about `embarch-study-designer`'s host
/// type schema (`embarch-study-designer` decision 12).
/// Downcastable so a caller can distinguish it from a transport failure —
/// the same idiom `StudyConflictError` already uses.
#[derive(Debug)]
pub struct SchemaVersionMismatch {
    pub api_version: u32,
    /// `None` when Core served no version at all, i.e. a Core built before
    /// the constant was split out onto `GET /status`.
    pub core_version: Option<u32>,
}

impl std::fmt::Display for SchemaVersionMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.core_version {
            Some(core) => write!(
                f,
                "embarch-study-designer host type schema mismatch: this embarch-api was built \
                 against v{}, embarch-core reports v{core}. A `Study` crosses this hop whole, so \
                 submitting across the drift would fail in whatever way serde happens to fail. \
                 Rebuild and redeploy whichever side is behind.",
                self.api_version
            ),
            None => write!(
                f,
                "embarch-study-designer host type schema mismatch: this embarch-api was built \
                 against v{}, and embarch-core served no version at all — it predates \
                 `GET /status` carrying one (`embarch-study-designer` decision 12's 2026-08-25 amendment). \
                 Redeploy embarch-core.",
                self.api_version
            ),
        }
    }
}

impl std::error::Error for SchemaVersionMismatch {}

#[derive(Debug, Serialize)]
struct FlashRequest<'a> {
    chip: &'a str,
    firmware_path: &'a str,
    format: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_address: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    probe_serial: Option<&'a str>,
    /// Omitted entirely when false, so a Core predating the field is
    /// unaffected by callers that don't ask for an erase.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    erase: bool,
    /// The build's `outpost-manifest.json`, when one sits beside the artifact.
    /// Omitted entirely when there is none, so a Core predating the field is
    /// unaffected.
    #[serde(skip_serializing_if = "Option::is_none")]
    manifest_path: Option<&'a str>,
}

/// The manifest an `embarch-outpost` build leaves beside its artifact
/// (`embarch-outpost` decision 9).
///
/// **Derived from the firmware path rather than passed in**, deliberately.
/// That decision says the engineer never handles this file, "because the
/// failure mode of forgetting is not a visible error but a silently
/// mislabelled trace, and the only reliable fix is for the manifest to travel
/// automatically with the build that produced it." A parameter is a thing a
/// caller can forget, and this crate has a dozen `flash` call sites across
/// `tools.rs`, `cli.rs` and `reflash.rs` — every one of which would be a place
/// to forget it. A rule applied here cannot be.
///
/// Absent for anything that is not an outpost-carrying build, including every
/// dev-bench flash, which is why nothing needs to know which board it is
/// talking to.
fn manifest_beside(firmware_path: &str) -> Option<PathBuf> {
    let path = Path::new(firmware_path).parent()?.join(OUTPOST_MANIFEST_FILE);
    path.is_file().then_some(path)
}

/// The name `embarch-outpost`'s post-link CMake step emits.
const OUTPOST_MANIFEST_FILE: &str = "outpost-manifest.json";

#[derive(Debug, Deserialize)]
pub struct FlashResponse {
    pub flashed: bool,
    pub chip: String,
}

#[derive(Debug, Serialize)]
struct ResetRequest<'a> {
    chip: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    probe_serial: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
pub struct ResetResponse {
    pub reset: bool,
}

/// Request body for [`CoreClient::set_dev_bench_link`] — mirrors Core's own
/// `SetDevBenchLinkRequest` (`embarch-core`'s `api.rs`). Write-only: no
/// `GET` echoes this back, unlike [`SignalLink`], so this is a plain
/// `Serialize` struct rather than a round-trip type.
#[derive(Debug, Serialize)]
struct SetDevBenchLinkRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    serial: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    interface: Option<u8>,
}

/// `embarch-core` decision 22's `POST /probes/enroll` — the
/// only sanctioned way to populate/update Core's local `known_boards`
/// table. Thin request/response wrappers, matching every other Core call in
/// this file: `embarch-api` holds no opinion on the shape of `known_boards`
/// itself, just relays this one call (decision 34's own rationale for why
/// this stays a two-layer wrapper rather than growing config of its own).
#[derive(Debug, Serialize)]
struct EnrollProbeRequest<'a> {
    role: &'a str,
    chip: &'a str,
    /// Picks which currently-attached probe to enroll when more than one is
    /// present (`embarch-core` decision 22's own doc comment;
    /// `embarch-topology` decision 15) — omitted, Core falls
    /// back to its original "exactly one attached" requirement. Added
    /// 2026-08-24: this field existed on Core's side since decision 15 but
    /// had no way to reach it through this client until `embarch-ui`'s
    /// Enroll tab needed to send exactly what its drag-and-drop UI already
    /// knows (`embarch-ui/milestone-1.md` §4.5).
    #[serde(skip_serializing_if = "Option::is_none")]
    probe_serial: Option<&'a str>,
}

// `Serialize` added 2026-08-24 alongside decision 5's amendment — the
// Enroll tab (`embarch-ui/milestone-1.md` §4.5) hands this straight back to
// the browser as JSON after a successful enroll.
#[derive(Debug, Serialize, Deserialize)]
pub struct EnrollProbeResponse {
    pub probe_serial: String,
    pub role: String,
    pub chip: String,
    /// The **probe/JTAG-read** hardware ID — the one Core read off the chip
    /// through the debug probe while enrolling, not anything the bench
    /// reported about itself. Unprefixed is the suite's default spelling for
    /// this concept (`embarch-core` decision 56, `tasks/api/044`); the only
    /// prefixed one is `probe_hardware_id` on `GET /dev-bench/hello`, where
    /// it sits beside `self_reported_hardware_id` and has to say which it is.
    pub hardware_id: String,
    pub confirmed_at_utc_ms: u64,
}

/// `embarch-core` decision 28's `POST /validate` — an
/// explicit, non-destructive re-check of an already-enrolled board's live
/// identity, the same check `flash`/`reset`/`run_study` already run
/// mid-attach, callable on its own without touching hardware otherwise.
#[derive(Debug, Serialize)]
struct ValidateRequest<'a> {
    role: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct ValidateResponse {
    pub role: String,
    pub probe_serial: String,
    pub chip: String,
    /// The **probe/JTAG-read** hardware ID, same concept and same spelling as
    /// `EnrollProbeResponse::hardware_id` — see `embarch-core` decision 56 for
    /// why this stays unprefixed while `GET /dev-bench/hello` prefixes its own.
    pub hardware_id: String,
    pub confirmed_at_utc_ms: u64,
    /// The instant *this* live check's hardware-ID compare passed — distinct
    /// from `confirmed_at_utc_ms` above, which names *enrolment* time and
    /// does not move on a re-check (`embarch-topology` decision 26,
    /// `embarch-core` decision 50). Flat, top-level, same as every other
    /// field here — `POST /validate`'s response shape is not
    /// `embarch_topology::hardware::Validation`'s nested `{ board, .. }`.
    ///
    /// `Option` so a Core older than `tasks/core/026` (predating this field)
    /// still parses, rather than every `validate` call failing at
    /// deserialization (`embarch-api` decision 58 — the crate's rule, not
    /// this field's exception; see `link_port_interface` and
    /// `study_designer_schema_version` for the same shape). `None` must never
    /// be presented as "validated at 1970" — a fabricated timestamp is worse
    /// than the parse error it replaces.
    #[serde(default)]
    pub validated_at_utc_ms: Option<u64>,
}

/// `POST /validate`'s `409 Conflict`/`503 Service Unavailable` body — the
/// enrolled board's live identity no longer matches what's recorded
/// (`"mismatch"`, `409`), or the enrolled probe could not be opened at all
/// (`"not_attached"`, `503`) — `embarch-core` decision 59 split what used to
/// be one shape rendered under one `topology mismatch` lead regardless of
/// which condition actually held.
#[derive(Debug, Deserialize)]
struct TopologyMismatchBody {
    role: String,
    probe_serial: String,
    chip: String,
    recorded_hardware_id: String,
    live_hardware_id: Option<String>,
    reason: String,
    /// `"not_attached"` or `"mismatch"` (`embarch-core` decision 59). Defaults
    /// to `"mismatch"` so a Core older than that decision — which only ever
    /// sent this shape for a genuine mismatch — still parses; never inferred
    /// from `live_hardware_id.is_none()` here, since that inference is
    /// exactly what decision 59 moved server-side into a real field.
    #[serde(default = "default_mismatch_kind")]
    kind: String,
    /// `None` on the `"not_attached"` arm (decision 59): the fix for a
    /// detached probe is a USB cable, not the Topology tab, and serving the
    /// same URL both times invited the same lead-conflation this shape
    /// exists to end.
    #[serde(default)]
    fix_it_url: Option<String>,
}

fn default_mismatch_kind() -> String {
    "mismatch".to_string()
}

/// Distinct error for `POST /validate`'s `409`/`503` — kept as its own
/// downcastable type (`StudyConflictError`'s own precedent above) so a
/// caller that wants to branch on "this specifically is a stale identity or
/// an absent probe, not some other failure" can
/// `e.downcast_ref::<TopologyMismatchError>()` for it, then branch further on
/// [`kind`](Self::kind) — never on `reason`'s wording (`embarch-core`
/// decision 59). `fix_it_url` is relayed onward when present, never
/// auto-opened here — `embarch-topology` decision 12's "opening the UI is the
/// caller's job," and this crate's own posture is to relay it as text, same
/// as `embarch-topology validate`'s own CLI never opening a browser.
#[derive(Debug)]
pub struct TopologyMismatchError {
    pub role: String,
    pub probe_serial: String,
    pub chip: String,
    pub recorded_hardware_id: String,
    pub live_hardware_id: Option<String>,
    pub reason: String,
    /// `"not_attached"` or `"mismatch"` — the field a caller branches on
    /// (`embarch-core` decision 59). Use [`Self::is_not_attached`] rather
    /// than comparing this string directly, so a caller need not know the
    /// literal spelling.
    pub kind: String,
    /// `None` on the `"not_attached"` arm — no fix-it URL applies to an
    /// unplugged probe.
    pub fix_it_url: Option<String>,
}

impl TopologyMismatchError {
    /// Whether this is the "probe couldn't be opened at all" condition
    /// (unplugged, ordinarily) rather than a genuine identity mismatch —
    /// the one distinction `embarch-core` decision 59 exists to carry as a
    /// field instead of leaving a caller to parse `reason`.
    pub fn is_not_attached(&self) -> bool {
        self.kind == "not_attached"
    }
}

impl std::fmt::Display for TopologyMismatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_not_attached() {
            write!(f, "probe not attached: {}", self.reason)
        } else if let Some(url) = &self.fix_it_url {
            write!(f, "topology mismatch: {} — fix it at {url}", self.reason)
        } else {
            write!(f, "topology mismatch: {}", self.reason)
        }
    }
}

impl std::error::Error for TopologyMismatchError {}

/// One entry from `GET /alerts` — mirrors `embarch_topology::hardware::
/// Alert`'s fields without depending on that crate's `hardware` feature
/// (this crate deliberately never links `probe-rs`/`serialport`,
/// `embarch-topology`'s own "no hardware knowledge" boundary).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertResponse {
    pub id: String,
    pub occurred_at_utc_ms: u64,
    pub role: String,
    pub probe_serial: String,
    pub chip: String,
    pub recorded_hardware_id: String,
    pub live_hardware_id: Option<String>,
    pub reason: String,
}

/// One entry from `GET /probes/enrolled` (`embarch-core` decision 22,
/// `link_port_serial` added decision 27) — every currently
/// enrolled board. Added 2026-08-24 for `embarch-ui`'s Dashboard/Topology
/// tabs (`embarch-ui` decision 5's amendment): reading this
/// over HTTP, rather than `embarch_topology::hardware::list_enrolled()`
/// in-process, is what keeps it correct when Core runs on a different
/// machine than whichever process is asking — the same "never link
/// probe-rs/serialport directly" rule §11 already states for this crate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnrolledBoardResponse {
    pub probe_serial: String,
    pub role: String,
    pub chip: String,
    /// The **probe/JTAG-read** hardware ID recorded at enrolment — not a live
    /// re-read, and not the bench's self-reported ID. `embarch-core` decision
    /// 56 settles the spelling; `surfaces.md`'s decision 54 covers why the timestamp beside it
    /// is enrolment time rather than freshness.
    pub hardware_id: String,
    pub confirmed_at_utc_ms: u64,
    #[serde(default)]
    pub link_port_serial: Option<String>,
    /// Which USB interface of the link-port device carries the runtime link,
    /// when the serial alone cannot say (a debug probe exposing two VCOM
    /// ports shares one USB serial across both) —
    /// `embarch_topology::hardware::EnrolledBoard::link_port_interface`'s own
    /// doc comment has the nRF54L15DK two-VCOM story that made this a real
    /// field, not a speculative one (`embarch-topology` decision 20). Added
    /// here 2026-09-07 after the mirror silently dropped it for a release.
    #[serde(default)]
    pub link_port_interface: Option<u8>,
}

/// One declared DUT signal link — `POST /signals` / `GET /signals`
/// (`embarch-topology` decision 18 and its 2026-08-25
/// amendment).
///
/// **A mirror of `embarch_topology::hardware::SignalLink`, not that type.**
/// The `hardware` module is behind that crate's `hardware` feature, which is
/// what pulls in `probe-rs`/`serialport` — the two dependencies this crate
/// deliberately never links (decisions 37, 38). Same reasoning
/// [`AlertResponse`] and [`EnrolledBoardResponse`] already state, and the
/// same obligation: the serde shape here has to match that type's byte for
/// byte, since this is what Core parses on the way in.
///
/// `Serialize` **and** `Deserialize` on one type rather than a request/
/// response pair, because the write and the read genuinely carry the same
/// thing: `declare_signal` is idempotent by name, so what comes back out of
/// `GET /signals` is exactly what went in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalLink {
    /// What a `Study` names when it taps this signal
    /// (`StreamSource::Signal { name }`). Unique within the table.
    pub name: String,
    /// The enrollment role the signal comes out of — `"dut"` for the
    /// outpost's UART.
    pub origin_role: String,
    pub direction: SignalDirection,
    pub route: SignalRoute,
}

/// Which way a signal travels. The outpost is `DutToHost` and TX-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SignalDirection {
    DutToHost,
    HostToDut,
    Bidirectional,
}

/// Where a signal currently goes. Mirrors
/// `embarch_topology::hardware::Route`, including its `tag = "kind"`
/// representation — the tag is what Core's `Json<SignalLink>` extractor
/// matches on, so it is part of the wire contract rather than a local
/// styling choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum SignalRoute {
    /// Straight to a serial port on the Core machine, **bypassing dev-bench
    /// entirely** — what the outpost uses today. `port_serial` is the
    /// bridge's own USB serial, one of [`SerialPortResponse::serial_number`].
    Direct { port_serial: String },
    /// Terminates on declared dev-bench pins, relayed over dev-bench's
    /// existing Core link.
    ViaDevBench { rx_pin: String, tx_pin: String },
}

/// One serial port from `GET /serial-ports` — mirrors
/// `embarch_topology::hardware::DetectedPort`, for the same
/// no-`probe-rs`/`serialport`-here reason [`SignalLink`] does.
///
/// This is **Core's** enumeration, and that distinction is the point: a
/// serial port on the machine running the asking process is not a serial port
/// on the machine running Core (`embarch-ui` decision 5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerialPortResponse {
    pub port_name: String,
    /// Which rule produced this entry. Always `"enumerated"` from
    /// `GET /serial-ports`, which narrows nothing — the VID-match values come
    /// from `/dev-bench/port`, which answers a different question.
    pub detected_by: String,
    pub vendor_id: Option<u16>,
    pub product_id: Option<u16>,
    /// What a `Route::Direct` is declared by. A port reporting `None` here
    /// cannot be declared as one, since nothing could resolve it later.
    pub serial_number: Option<String>,
    pub product: Option<String>,
    pub interface: Option<u8>,
}

/// `GET /study/{study_id}/steps`' body — every step the study recorded, with
/// the two edges of the window embarch-core waited for each across.
///
/// The stamps are **Core's own arrival times**, not the DUT's or dev-bench's:
/// `StepResult` carries no timestamp, deliberately. Read off `events.json` on
/// disk rather than out of Core's job registry, because a study outlives the
/// Core process that ran it and a post-hoc reader is exactly the caller here.
#[derive(Debug, Clone, Deserialize)]
pub struct StudySteps {
    #[serde(default)]
    pub study_name: Option<String>,
    /// Whether every step carries both of its stamps. False for a study run
    /// before 2026-08-27, when Core recorded neither — the one thing a caller
    /// has to branch on.
    pub timed: bool,
    pub steps: Vec<StudyStepEntry>,
}

/// One step, reduced to what a reader placing it on a timeline needs.
#[derive(Debug, Clone, Deserialize)]
pub struct StudyStepEntry {
    pub index: usize,
    pub step_name: String,
    /// `"Pass"`, `"Fail"` or `"TimedOut"`. Core splits `Outcome`'s
    /// externally-tagged JSON so a caller does not re-implement its wire
    /// shape to find out which of the three it was.
    pub outcome: String,
    /// dev-bench's own words, on a `Fail` and never otherwise.
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub delay_before_ms: Option<u32>,
    #[serde(default)]
    pub started_utc_ms: Option<u64>,
    #[serde(default)]
    pub ended_utc_ms: Option<u64>,
}

/// `GET /study/{study_id}/streams`' body — what a study's taps captured, and
/// **why a trace has no names when it has none**
/// (`embarch-core` decision 30(c)'s 2026-08-26 amendment).
#[derive(Debug, Clone, Deserialize)]
pub struct StudyStreamIndex {
    pub streams: Vec<StudyStreamEntry>,
}

/// One declared tap, as the study's own stream index reports it.
#[derive(Debug, Clone, Deserialize)]
pub struct StudyStreamEntry {
    pub id: u8,
    pub name: String,
    pub encoding: StreamEncoding,
    /// Whether a decoded rendering exists — i.e. whether
    /// [`CoreClient::get_study_stream`] with `raw = false` hands back a
    /// decoded rendering or the raw bytes.
    pub rendered: bool,
    /// Why this tap's rendering is missing, incomplete, **unnamed** or
    /// **untimed**.
    ///
    /// The one field this whole endpoint exists for. `None` is "nothing to
    /// report"; `Some` on an `OutpostTrace` tap means the trace decoded into
    /// structure but is missing its names, its times, or both — and a caller
    /// must not present it as a complete one (`embarch-ui` decision 10,
    /// trace half). **Prose, for a person:** branch on the two booleans
    /// below, never on this text.
    #[serde(default)]
    pub note: Option<String>,
    /// Whether an applicable manifest named this trace's threads, ISRs and
    /// markers (`embarch-outpost` decision 9). `None` from a Core
    /// that predates the field, or on a tap where the question is meaningless.
    #[serde(default)]
    pub named: Option<bool>,
    /// Whether this trace's frames carry Core's own receipt time — the clock
    /// that **places** a trace against the study's other streams, alongside
    /// the DUT's own per-record `cycles` that measures it
    /// (`embarch-outpost` decisions 4, 17). `Some(false)` is a
    /// trace with no placement: real, and still fully measurable on the DUT's
    /// clock.
    #[serde(default)]
    pub timed: Option<bool>,
    /// Whether the firmware kept **itself** out of this trace — no record of
    /// the outpost's own drain thread or its own UART's interrupt
    /// (`embarch-outpost` decision 19,
    /// `CONFIG_EMBARCH_OUTPOST_TRACE_SELF=n`, the default).
    ///
    /// A third independent fact beside `named`/`timed`, and the only one the
    /// *firmware* decides. `Some(true)` means intervals covered by no lane are
    /// the instrument's own rather than unexplained, and a caller must say so
    /// rather than presenting the timeline as an account of everything the CPU
    /// did. `None` from a Core that predates the field.
    #[serde(default)]
    pub self_excluded: Option<bool>,
}

impl StudyStreamEntry {
    /// Whether an applicable manifest named this trace.
    ///
    /// Falls back to the old conjunction for a Core that predates
    /// [`named`](Self::named) — which was correct while `note` could only ever
    /// mean "unnamed". It stopped being correct when a trace gained a second
    /// way to be incomplete: an untimed trace carries a note and is *named*,
    /// and the fallback calls it unnamed. Hence the field.
    pub fn is_named(&self) -> bool {
        self.named.unwrap_or(self.rendered && self.note.is_none())
    }

    /// Whether this trace's frames carry receipt times. `None` — a Core that
    /// predates the field — reads as **not timed**: a caller that assumed
    /// otherwise would draw a millisecond axis over frame indices.
    pub fn is_timed(&self) -> bool {
        self.timed.unwrap_or(false)
    }
}

/// `GET /dev-bench/port`'s success body (`embarch-core` spec.md) —
/// which serial port `embarch-dev-bench` is on right now. Every field but
/// `port_name`/`detected_by` is nullable, matching Core's own endpoint doc:
/// an explicitly-configured port need not be USB-enumerable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevBenchPortResponse {
    pub port_name: String,
    pub detected_by: String,
    pub vendor_id: Option<u16>,
    pub product_id: Option<u16>,
    pub serial_number: Option<String>,
    pub product: Option<String>,
    pub interface: Option<u8>,
}

/// `GET /logs/recent`'s body (`embarch-core` spec.md) — plain lines
/// exactly as `tracing_subscriber`'s own formatter wrote them, no
/// server-side structuring/filtering (`embarch-ui` decision 7's
/// resolution of that open question).
#[derive(Debug, Deserialize)]
struct LogsRecentResponse {
    lines: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct SerialLogResponse {
    pub port: String,
    pub lines: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ResolveChipRequest<'a> {
    soc: &'a str,
}

#[derive(Debug, Deserialize)]
struct ResolveChipResponse {
    chip: String,
}

/// `POST /study`'s success (200) body — this repo's `spec.md`: `{ "study_id": "<uuid-string>", "status": "accepted" }`. Only
/// `study_id` is modeled — `run_study`/`run-study` return `{ study_id }`
/// verbatim (per spec) and have no use for `status`, which is always
/// `"accepted"` on a 200 anyway; serde ignores the extra field on
/// deserialize.
#[derive(Debug, Deserialize)]
pub struct PostStudyResponse {
    pub study_id: String,
}

/// The two out-of-band run parameters `POST /study` accepts as **query
/// parameters** (`embarch-core` decision 31's amendment, decision 40).
///
/// Neither can ride inside the `Study` body: `embarch-study-designer`
/// decision 40 settles that reflash is "a run parameter, not a study
/// field", so a saved study would otherwise carry a reflash instruction into
/// every later re-read of its own results. Keeping them out of the body also
/// leaves `Study`'s bytes — and therefore `steps_crc`/`streams_crc` — exactly
/// as they were.
///
/// [`Default`] is "nothing was flashed, nothing is waived", which is the
/// shape every caller that does not orchestrate a flash wants and the
/// behavior every caller had before this existed.
#[derive(Debug, Default, Clone)]
pub struct StudyRunOptions {
    /// Proceed past a version requirement this run does not satisfy. The
    /// override is **recorded** in `StudyResult.provenance.overrides`, never
    /// silently honoured.
    pub allow_version_mismatch: bool,
    /// What this run just flashed onto the DUT, if it did. Its presence is
    /// what lets Core write `VersionSource::FlashedThisRun` honestly —
    /// `POST /flash` and `POST /study` are separate calls with nothing
    /// linking them, so the process that sequenced both is the only one that
    /// can say so (`embarch-core` decision 31's implementation
    /// note).
    pub flashed_firmware_version: Option<String>,
}

impl StudyRunOptions {
    /// The `?k=v` suffix for `POST /study`, empty when nothing is set — so a
    /// default-options submit is byte-identical to the URL every caller sent
    /// before these existed.
    fn query_suffix(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.allow_version_mismatch {
            parts.push("allow_version_mismatch=1".to_string());
        }
        if let Some(version) = &self.flashed_firmware_version {
            parts.push(format!("flashed_firmware_version={}", urlencode(version)));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("?{}", parts.join("&"))
        }
    }
}

/// Percent-encodes everything outside the unreserved set. A version string
/// is `git describe` output in practice — `g1a2b3c-dirty`, all unreserved —
/// but it is free-form and reaches this from a config-declared command, so
/// encoding it is not optional. Hand-rolled rather than adding a dependency
/// for one call site.
fn urlencode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// `GET /dev-bench/hello`'s body — mirrors `embarch-core`'s own
/// `study::HelloAckInfo` (`src/study.rs`) field-for-field, not
/// `embarch_topology::hardware`'s comparison types, since Core is what
/// actually serializes this route and this crate can't link
/// `embarch-topology`'s `hardware` feature to check the two agree (see
/// `SignalLink`'s own doc comment above for the same constraint on a
/// different type). `firmware_version` is what the bench currently running
/// actually reports, which is the only version in this suite that is
/// genuinely read back off the thing it describes.
///
/// **`link_identity` is the whole point of this route being served at
/// all** — `self_reported_hardware_id` compared against `probe_hardware_id`
/// is the only place in the suite that surfaces both the JTAG-read and the
/// bench's own self-reported identity, and how they relate, as data. It is a
/// stable string (`"match"`/`"mismatch"`/`"not-reported"`/`"undeclared"`),
/// deliberately not a `bool`: today's real answer for every chip is
/// `"undeclared"` (`embarch-core` §3 decision 35), and collapsing that to
/// `compatible: true`-shaped success would make an unverified board look
/// confirmed. A caller that only reads `compatible` and ignores this field
/// has silently thrown away the one fact this endpoint exists to report.
///
/// Field named `self_reported_hardware_id`, not `hardware_id`, because Core
/// decision 47 (2026-09-07, `tasks/core/020`) renamed it after finding that
/// name collided with the JTAG-read `hardware_id` served by
/// `/probes/enroll`, `/probes/enrolled` and `POST /validate` — this route's
/// own `probe_hardware_id` field is that same JTAG-read value, spelled
/// differently *within this one route* on purpose (see those three structs'
/// own `hardware_id` fields — this route never used the ambiguous name so
/// nothing here needed changing). **That rename is now cancelled rather than
/// pending**: `embarch-core` decision 56 (`tasks/api/044`) settles unprefixed
/// `hardware_id` as the suite's name for the probe-read ID, and keeps the
/// `probe_hardware_id` prefix confined to this route, where the two IDs are
/// neighbours in one body.
///
/// **The three identity fields are `Option<String>` with `#[serde(default)]`,
/// per `embarch-api` decision 58** — every response field this crate
/// deserializes that Core may not yet send is optional, because the rename
/// above (`embarch-core` decision 47) happened *after* this route already
/// existed: a Core that predates the rename serves `hardware_id`, not
/// `self_reported_hardware_id`, and does not serve `link_identity` or
/// `probe_hardware_id` under those spellings at all. A bare required field
/// here would make `serde` fail the whole response on a missing key against
/// exactly that Core — turning the one route built to answer *is the board
/// on the link the board the probe verified?* into a deserialization error
/// instead of an answer. `None` means "this Core did not send it", which is
/// a fact about the Core, not about the bench — never conflate it with the
/// bench's own `"not-reported"` (a real, declared answer) — see the two
/// rendering states in `embarch-api decision 59`'s tool.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct HelloAckResponse {
    pub schema_version: u32,
    pub compatible: bool,
    pub firmware_version: String,
    #[serde(default)]
    pub self_reported_hardware_id: Option<String>,
    #[serde(default)]
    pub link_identity: Option<String>,
    #[serde(default)]
    pub probe_hardware_id: Option<String>,
}

/// Renders a [`HelloAckResponse`] for a reader, per `embarch-api` decision
/// 59 — the rendering call settled after this unit was refused once for
/// treating a missing field as a pass.
///
/// **Two states that must not be reachable from each other:**
///
/// - **Complete** (all three identity fields present): each is rendered
///   verbatim, under its own label, exactly as the bytes Core sent —
///   `"not-reported"`/`"undeclared"` reach the reader unaltered, because
///   they are the *board's* own answers and are real bench results, not a
///   client-side judgement. A trailing note says so explicitly.
/// - **Incomplete** (any of the three is `None`): the output **leads** with
///   a line naming the cross-check unavailable and which field(s) were
///   absent, points at `embarch-core` decision 47 (the rename that an older
///   Core predates) as the known cause and `embarch-api` decision 58 as why
///   the call tolerates it instead of failing, and only then may render the
///   fields Core did report — never above that line, never implying a
///   comparison was made.
///
/// This function never computes a verdict of its own from the two hardware
/// ids — `link_identity` is Core's own answer to the cross-check and is
/// surfaced, not replaced (`embarch-topology` decision 20's failure mode,
/// named in `embarch-api` decision 59).
///
/// A `None` field renders as exactly one thing, a sentence, never a token:
/// never an empty string, `null`, `-`, `"unknown"`, and — the one that
/// actually matters — **never `"not-reported"`**, a different fact with a
/// different cause (the *board* declining to state an identity, versus
/// *this Core* not having the field at all).
pub fn render_hello_ack(info: &HelloAckResponse) -> String {
    fn field_line(label: &str, value: &Option<String>) -> String {
        match value {
            Some(v) => format!("{label}: {v}"),
            None => format!("{label}: this Core did not send this field."),
        }
    }

    let missing: Vec<&str> = [
        ("self_reported_hardware_id", info.self_reported_hardware_id.is_none()),
        ("probe_hardware_id", info.probe_hardware_id.is_none()),
        ("link_identity", info.link_identity.is_none()),
    ]
    .into_iter()
    .filter_map(|(name, is_missing)| is_missing.then_some(name))
    .collect();

    let core_fields = format!(
        "schema_version: {}\ncompatible: {}\nfirmware_version: {}",
        info.schema_version, info.compatible, info.firmware_version
    );
    let identity_fields = format!(
        "{}\n{}\n{}",
        field_line("self_reported_hardware_id", &info.self_reported_hardware_id),
        field_line("probe_hardware_id", &info.probe_hardware_id),
        field_line("link_identity", &info.link_identity),
    );

    if missing.is_empty() {
        format!(
            "Identity cross-check: complete — self_reported_hardware_id, probe_hardware_id \
             and link_identity were all reported by this Core.\n\n{core_fields}\n{identity_fields}\n\n\
             Note: \"not-reported\" and \"undeclared\" above are the board's own answers, not \
             confirmations — read link_identity itself; never infer a pass from compatible or \
             from the two ids' mere presence."
        )
    } else {
        format!(
            "Identity cross-check: UNAVAILABLE — this Core did not send {}.\n\n\
             embarch-core decision 47 (tasks/core/020) renamed hardware_id to \
             self_reported_hardware_id; a Core older than that rename does not serve these \
             fields under these names. embarch-api decision 58 is why this client tolerates the \
             missing field(s) rather than failing the call outright, and decision 59 is why this \
             tool renders \"unavailable\" here rather than a partial pass.\n\n\
             Fields this Core did report (not a cross-check — the comparison itself is \
             unavailable):\n{core_fields}\n{identity_fields}",
            missing.join(", "),
        )
    }
}

/// Distinct error for `GET /dev-bench/hello`'s `409 Conflict`: a study is
/// already in flight on Core and this route refused to race it for the
/// link, rather than actually attempting (and failing) the handshake. Kept
/// as its own downcastable type (`StudyConflictError`'s own precedent above)
/// so a caller — `embarch-api`'s MCP tool in particular — can send an
/// operator to "wait for the study" rather than "the bench is unplugged or
/// broken", which is what [`DevBenchHandshakeError`] means instead.
#[derive(Debug)]
pub struct DevBenchBusyError {
    pub message: String,
}

impl std::fmt::Display for DevBenchBusyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for DevBenchBusyError {}

/// Distinct error for `GET /dev-bench/hello`'s `502 Bad Gateway`: the
/// `Hello`/`HelloAck` handshake itself failed — dev-bench didn't answer, a
/// declared identity mismatch was found, or its firmware reported itself
/// incompatible. Nothing here says a study is running; this is the "go
/// look at the bench" case, [`DevBenchBusyError`]'s opposite.
#[derive(Debug)]
pub struct DevBenchHandshakeError {
    pub message: String,
}

impl std::fmt::Display for DevBenchHandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for DevBenchHandshakeError {}

/// `POST /study`'s `409 Conflict` body: `{"study_id": "<uuid-string>"}`
/// naming the study already in-flight.
#[derive(Debug, Deserialize)]
struct StudyConflictBody {
    study_id: String,
}

/// Distinct error for `POST /study`'s `409 Conflict` — Core already has a
/// study in flight. Kept as its own type (rather than folded into a
/// generic `anyhow!(...)` string) so a caller that wants to branch on "a
/// study is already running" specifically (as opposed to any other error)
/// can `e.downcast_ref::<StudyConflictError>()` for it.
#[derive(Debug)]
pub struct StudyConflictError {
    pub study_id: String,
}

impl std::fmt::Display for StudyConflictError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "embarch-core already has a study in-flight (study_id: {})",
            self.study_id
        )
    }
}

impl std::error::Error for StudyConflictError {}

/// `GET /study/{study_id}`'s body — this repo's `spec.md`. `status` is
/// left as a plain `String` (matching how `StatusResponse.status` above is
/// already handled) rather than a closed enum, since this is a
/// loosely-typed pass-through of whatever Core reports.
#[derive(Debug, Deserialize)]
pub struct StudyStatusResponse {
    pub status: String,
    pub current_step: Option<u32>,
    pub total_steps: Option<u32>,
    pub result: Option<StudyResult>,
    pub reason: Option<String>,
}

/// Core's structured non-2xx error body (`embarch-core` decision 12): `{"code": "...", "message": "...", "cause": "..."}`. Not
/// every Core error response uses this shape yet (existing endpoints still
/// return plain text, per `send`'s doc comment above) — so parsing this is
/// attempted, with a plain-text fallback, rather than assumed.
#[derive(Debug, Deserialize)]
struct CoreErrorBody {
    code: Option<String>,
    message: Option<String>,
    cause: Option<String>,
}

impl CoreClient {
    pub fn new(config: &CoreConfig) -> Result<CoreClient> {
        let token = config.resolve_token()?;
        let client = reqwest::Client::builder()
            .build()
            .context("failed to build reqwest client")?;

        let address = if config.is_auto() {
            Address::Auto {
                host: config.host.clone(),
                port: config.port,
            }
        } else {
            Address::Declared(config.base_url.trim_end_matches('/').to_string())
        };

        Ok(CoreClient {
            address: Arc::new(address),
            resolved: Arc::new(OnceCell::new()),
            token,
            client,
            status_timeout: Duration::from_secs(config.status_timeout_secs),
            reset_timeout: Duration::from_secs(config.reset_timeout_secs),
            flash_timeout: Duration::from_secs(config.flash_timeout_secs),
            serial_timeout: Duration::from_secs(config.serial_timeout_secs),
            study_timeout: Duration::from_secs(config.study_timeout_secs),
        })
    }

    /// Core's base URL, discovering it on first use if `base_url = "auto"`.
    ///
    /// The failure message names every candidate tried and what each one
    /// said, because "couldn't find Core" is useless on its own — the useful
    /// information is whether nothing was listening, or something answered
    /// and wasn't Core.
    async fn resolved_address(&self) -> Result<&(String, TopologyClass)> {
        self.resolved
            .get_or_try_init(|| async {
                let (host, port) = match self.address.as_ref() {
                    // A declared address is exactly the same-machine dev
                    // workflow (`embarch-dev-workflow.md` §2) `flash` has
                    // always assumed for it: no probing, and treated as
                    // `Local` so a path sent to Core stays a plain path,
                    // unchanged from before this decision existed.
                    Address::Declared(url) => {
                        return Ok::<(String, TopologyClass), anyhow::Error>((
                            url.clone(),
                            TopologyClass::Local,
                        ))
                    }
                    Address::Auto { host, port } => (host.as_deref(), *port),
                };

                // `embarch-topology` decisions 2, 3: live, in-process,
                // every call — this crate no longer owns any of the WSL2/
                // gateway/probe I/O itself (formerly `env.rs`/`probe.rs`/this
                // module's own `topology.rs` mirror).
                let resolved = embarch_topology::software::resolve_software_topology(port, host, None).await;

                match resolved.winner {
                    Some(candidate) => {
                        tracing::info!(
                            "embarch-core found at {} ({})",
                            candidate.base_url,
                            candidate.class.as_str()
                        );
                        Ok((candidate.base_url, candidate.class))
                    }
                    None => {
                        let tried = resolved
                            .attempts
                            .iter()
                            .map(|a| {
                                let why = match a.outcome {
                                    ProbeOutcome::Unreachable => "nothing listening".to_string(),
                                    ProbeOutcome::NotCore { status } => format!(
                                        "answered HTTP {status}, but isn't embarch-core"
                                    ),
                                    ProbeOutcome::Core { .. } => unreachable!("a hit would win"),
                                };
                                format!("\n  {} ({}) — {why}", a.candidate.base_url, a.candidate.class.as_str())
                            })
                            .collect::<String>();
                        Err(anyhow!(
                            "embarch-core not found (base_url = \"auto\"). Tried:{tried}\n\
                             Start embarch-core, or set [core].base_url to an explicit URL \
                             (or [core].host, for a Core on another machine)."
                        ))
                    }
                }
            })
            .await
    }

    pub(crate) async fn base_url(&self) -> Result<&str> {
        Ok(&self.resolved_address().await?.0)
    }

    /// The shared `reqwest` client, for the one caller that builds its
    /// request outside this file: `study_events`, which streams a body
    /// instead of parsing one and must set no request timeout. It carries
    /// **no** credential — the token is applied by [`CoreClient::dispatch`],
    /// which that caller goes through like every other route.
    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.client
    }

    /// **The one place an outbound request is authenticated and sent.**
    ///
    /// Every route in this client hands its `RequestBuilder` here — the
    /// ones that just want JSON back, via [`CoreClient::send`], and equally
    /// the ones that read a status themselves (a `404` that means "not
    /// enrolled", a `409` that carries a topology mismatch) or that stream.
    /// That makes `Authorization: Bearer …` unconditional **by
    /// construction rather than by convention**, the shape `json_out` takes
    /// for `schema_version` (`embarch-api` decisions 50, 55), and
    /// `every_outbound_request_is_sent_through_the_one_funnel` fails if a
    /// second send site appears anywhere in the crate.
    ///
    /// `timeout: None` is the streaming case and nothing else: `reqwest`'s
    /// per-request timeout covers the body too, so applying one to an SSE
    /// subscription would cut a healthy stream off — see
    /// [`CoreClient::open_study_events`], which bounds itself per read
    /// instead.
    ///
    /// ***Rejected: `default_headers` on the `ClientBuilder`.*** It would
    /// attach the token to every request this `reqwest::Client` makes
    /// rather than to every request *this client's routes* make, and those
    /// are not the same set once anything else is built on the handle
    /// `http()` already hands out. Per-route attachment also stays
    /// observable: the mocked sweep can assert the header on the wire for
    /// each route, which a builder default makes invisible at every call
    /// site.
    pub(crate) async fn dispatch(
        &self,
        request: reqwest::RequestBuilder,
        timeout: Option<Duration>,
    ) -> Result<reqwest::Response> {
        let request = request.bearer_auth(&self.token);
        let request = match timeout {
            Some(timeout) => request.timeout(timeout),
            None => request,
        };
        request
            .send()
            .await
            .context("request to embarch-core failed")
    }

    /// The winning topology class — `Local` for a declared address (no
    /// probing done), otherwise whichever candidate actually answered.
    /// `flash`'s only consumer: a `WslHost`/`Remote` Core can't be assumed
    /// to share a filesystem with this process (decision 15's 2026-08-18
    /// finding — a Session-0-service Core can't reach a `WslHost`'s
    /// `\\wsl.localhost` UNC path at all), so those classes get the
    /// artifact's bytes instead of a path.
    async fn topology_class(&self) -> Result<TopologyClass> {
        Ok(self.resolved_address().await?.1)
    }

    /// Core's error responses are plain-text bodies (axum's IntoResponse for
    /// `(StatusCode, String)`), not JSON — so non-2xx bodies must be read as
    /// text, never parsed as JSON, or Core's actual error message is lost.
    ///
    /// The default funnel: any status but 2xx is an error. A route that
    /// gives a particular status its own meaning goes through
    /// [`CoreClient::dispatch`] directly and reads the status itself.
    async fn send<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
        timeout: Duration,
    ) -> Result<T> {
        let response = self.dispatch(request, Some(timeout)).await?;

        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<no response body>".to_string());
            return Err(anyhow!("embarch-core returned {status}: {body}"));
        }

        response
            .json::<T>()
            .await
            .context("failed to parse embarch-core's response as JSON")
    }

    /// `send`'s counterpart for a route that answers `204 No Content`.
    ///
    /// Needed because [`CoreClient::send`] always parses a body, and axum's
    /// `StatusCode`-only responses have none — `send::<()>` would fail on the
    /// empty body rather than on anything real.
    async fn send_no_content(
        &self,
        request: reqwest::RequestBuilder,
        timeout: Duration,
    ) -> Result<()> {
        let response = self.dispatch(request, Some(timeout)).await?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<no response body>".to_string());
        Err(anyhow!("embarch-core returned {status}: {body}"))
    }

    /// Formats a non-2xx `/study/*` error body: Core's new `{code, message,
    /// cause}` shape (`embarch-core` decision 12) if the body
    /// parses as one, else the raw text — same fallback posture as `send`'s
    /// doc comment above, since not every endpoint has moved to the
    /// structured shape yet.
    fn format_study_error(status: reqwest::StatusCode, body: &str) -> String {
        match serde_json::from_str::<CoreErrorBody>(body) {
            Ok(CoreErrorBody { code, message: Some(message), cause }) => {
                let code = code.as_deref().unwrap_or("error");
                match cause {
                    Some(cause) => format!("embarch-core returned {status} [{code}]: {message}\ncause: {cause}"),
                    None => format!("embarch-core returned {status} [{code}]: {message}"),
                }
            }
            _ => format!("embarch-core returned {status}: {body}"),
        }
    }

    pub async fn status(&self) -> Result<StatusResponse> {
        let url = format!("{}/status", self.base_url().await?);
        self.send(self.client.get(url), self.status_timeout).await
    }

    /// `firmware_path` is always a path *this process* can read — the
    /// WSL2-local artifact path, or a CLI `--firmware-path` override, never
    /// a UNC form the caller computed for Core. What gets sent to Core
    /// depends on the resolved topology (decision 15's 2026-08-18
    /// finding): `Local` (same machine, or a declared dev-workflow address)
    /// sends the path as JSON, unchanged from before this decision — Core
    /// can just open it. `WslHost`/`Remote` — Core running natively on the
    /// Windows host of this WSL2 guest, or on a genuinely separate machine
    /// — reads the file here and uploads its bytes as `multipart/form-data`
    /// instead, since a Core running as an installed Windows service (as
    /// opposed to a foreground `run`) has no access to this process's
    /// `\\wsl.localhost` share at all — confirmed by direct A/B test, not
    /// assumed. This is strictly more general than the UNC-path mechanism it
    /// replaces for these classes: it works identically whether Core is
    /// foreground or an installed service, so callers no longer need to
    /// compute or send a `firmware_path_for_core`-style UNC form at all.
    /// `base_address` (`embarch-core` decision 18) is only
    /// meaningful for `format = "bin"` — silently ignored by Core otherwise,
    /// same posture that decision's own text documents at Core's single call
    /// site, so a caller that always passes the same value regardless of
    /// format doesn't have to special-case it here either.
    ///
    /// `erase` requests a full chip erase before writing (`west flash
    /// --erase`), rather than erasing only the sectors the image covers.
    /// Threaded through both transports — the JSON body and the multipart
    /// upload — since a WSL-host Core takes the latter, and an erase that
    /// silently applied on one path but not the other would be worse than
    /// not offering it.
    ///
    /// `probe_serial` (`embarch-core` decision 9) disambiguates
    /// which attached debug probe to use when more than one is present —
    /// designed there well ahead of a real second probe existing, and never
    /// actually threaded through from this side until dev-bench's own
    /// flashing pipeline made that real: `open_first_probe()` picking
    /// whichever probe happens to enumerate first is a real, reproducible
    /// failure ("interface Jtag must be selected... currently using
    /// interface Swd") the moment a DUT's probe and dev-bench's own probe
    /// are both plugged in and this is omitted.
    pub async fn flash(
        &self,
        chip: &str,
        firmware_path: &str,
        format: &str,
        base_address: Option<&str>,
        probe_serial: Option<&str>,
        erase: bool,
    ) -> Result<FlashResponse> {
        let url = format!("{}/flash", self.base_url().await?);
        let manifest = manifest_beside(firmware_path);
        if let Some(manifest) = manifest.as_deref() {
            tracing::debug!(
                manifest = %manifest.display(),
                "an outpost manifest sits beside this artifact; sending it with the flash"
            );
        }

        match self.topology_class().await? {
            TopologyClass::Local => {
                let manifest_path = manifest.as_deref().and_then(Path::to_str);
                let body = FlashRequest {
                    chip,
                    firmware_path,
                    format,
                    base_address,
                    probe_serial,
                    erase,
                    manifest_path,
                };
                self.send(self.client.post(url).json(&body), self.flash_timeout)
                    .await
            }
            TopologyClass::WslHost | TopologyClass::Remote => {
                let path = Path::new(firmware_path);
                let bytes = tokio::fs::read(path).await.with_context(|| {
                    format!("failed to read firmware artifact at {}", path.display())
                })?;
                let file_name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("firmware.bin")
                    .to_string();
                let mut form = reqwest::multipart::Form::new()
                    .text("chip", chip.to_string())
                    .text("format", format.to_string());
                if let Some(base_address) = base_address {
                    form = form.text("base_address", base_address.to_string());
                }
                if let Some(probe_serial) = probe_serial {
                    form = form.text("probe_serial", probe_serial.to_string());
                }
                if erase {
                    form = form.text("erase", "true");
                }
                // Uploaded as bytes for the same reason the artifact is: a
                // `WslHost`/`Remote` Core cannot open a path on this side.
                if let Some(manifest) = manifest.as_deref() {
                    let json = tokio::fs::read_to_string(manifest).await.with_context(|| {
                        format!("failed to read the outpost manifest at {}", manifest.display())
                    })?;
                    form = form.text("manifest", json);
                }
                let form = form.part(
                    "firmware",
                    reqwest::multipart::Part::bytes(bytes).file_name(file_name),
                );
                self.send(self.client.post(url).multipart(form), self.flash_timeout)
                    .await
            }
        }
    }

    pub async fn reset(&self, chip: &str, probe_serial: Option<&str>) -> Result<ResetResponse> {
        let url = format!("{}/reset", self.base_url().await?);
        let body = ResetRequest { chip, probe_serial };
        self.send(self.client.post(url).json(&body), self.reset_timeout)
            .await
    }

    /// `POST /probes/enroll` (`embarch-core` decision 22,
    /// decision 34) — records which physical
    /// board `role`'s probe is. `probe_serial` picks a specific attached
    /// probe when more than one is present (given, e.g. by a drag-and-drop
    /// UI that already knows exactly which card was dropped); omitted,
    /// Core falls back to its "exactly one attached" requirement. Reuses
    /// `reset_timeout`: like `reset`, this is one probe attach plus a
    /// couple of memory reads, not a multi-second flash.
    pub async fn enroll_probe(&self, role: &str, chip: &str, probe_serial: Option<&str>) -> Result<EnrollProbeResponse> {
        let url = format!("{}/probes/enroll", self.base_url().await?);
        let body = EnrollProbeRequest { role, chip, probe_serial };
        self.send(self.client.post(url).json(&body), self.reset_timeout)
            .await
    }

    /// `POST /validate` (`embarch-core` decision 28) — the
    /// explicit, non-destructive counterpart to the live re-check
    /// `flash`/`reset`/`run_study` already run mid-attach: same underlying
    /// `embarch_topology::hardware::validate_role` call, callable on its own
    /// at any time. Reuses `reset_timeout`, same reasoning as `enroll_probe`
    /// above: one probe attach plus a couple of memory reads, not a
    /// multi-second flash.
    pub async fn validate(&self, role: &str) -> Result<ValidateResponse> {
        let url = format!("{}/validate", self.base_url().await?);
        let response = self
            .dispatch(
                self.client.post(url).json(&ValidateRequest { role }),
                Some(self.reset_timeout),
            )
            .await?;

        let status = response.status();
        if status.is_success() {
            return response
                .json::<ValidateResponse>()
                .await
                .context("failed to parse embarch-core's response as JSON");
        }

        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<no response body>".to_string());

        if status == reqwest::StatusCode::CONFLICT || status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
            return match serde_json::from_str::<TopologyMismatchBody>(&body) {
                Ok(m) => Err(anyhow::Error::new(TopologyMismatchError {
                    role: m.role,
                    probe_serial: m.probe_serial,
                    chip: m.chip,
                    recorded_hardware_id: m.recorded_hardware_id,
                    live_hardware_id: m.live_hardware_id,
                    reason: m.reason,
                    kind: m.kind,
                    fix_it_url: m.fix_it_url,
                })),
                Err(_) => Err(anyhow!(
                    "embarch-core returned {status} (a topology mismatch or an unattached \
                     probe), but its response body didn't parse as expected: {body}"
                )),
            };
        }

        if status == reqwest::StatusCode::NOT_FOUND {
            // Core's 404 body is already a complete, human-readable message
            // (`embarch-topology::hardware::NotEnrolled`'s own `Display`) —
            // relayed verbatim rather than wrapped in more prose.
            return Err(anyhow!("{body}"));
        }

        Err(anyhow!("embarch-core returned {status}: {body}"))
    }

    /// `GET /alerts` (`embarch-core` decision 28) — recent
    /// topology-mismatch alerts from Core's durable log
    /// (`embarch_topology::hardware::recent_alerts`). Reuses
    /// `status_timeout`: a pure local-file read on Core's side, no hardware
    /// touched.
    pub async fn alerts(&self, limit: usize) -> Result<Vec<AlertResponse>> {
        let url = format!("{}/alerts", self.base_url().await?);
        let request = self.client.get(url).query(&[("limit", limit.to_string())]);
        self.send(request, self.status_timeout).await
    }

    /// `GET /probes/enrolled` (`embarch-core` decision 22) —
    /// every currently enrolled board. Reuses `status_timeout`: a pure read
    /// of `embarch-topology`'s own storage on Core's side, no hardware
    /// touched — same posture as `alerts` above.
    pub async fn list_enrolled(&self) -> Result<Vec<EnrolledBoardResponse>> {
        let url = format!("{}/probes/enrolled", self.base_url().await?);
        self.send(self.client.get(url), self.status_timeout).await
    }

    /// `GET /dev-bench/port` (`embarch-core` spec.md) — which
    /// serial port `embarch-dev-bench` is on right now, if any. Core's own
    /// `404` for "no port matches" is an expected state (bench unplugged),
    /// not a Core failure, so it's surfaced as `Ok(None)` rather than an
    /// error — a caller that wants to render "not connected" doesn't need
    /// to match on an error string to do it.
    pub async fn dev_bench_port(&self) -> Result<Option<DevBenchPortResponse>> {
        let url = format!("{}/dev-bench/port", self.base_url().await?);
        let response = self
            .dispatch(self.client.get(url), Some(self.status_timeout))
            .await?;

        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status.is_success() {
            return response
                .json::<DevBenchPortResponse>()
                .await
                .map(Some)
                .context("failed to parse embarch-core's response as JSON");
        }

        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<no response body>".to_string());
        Err(anyhow!("embarch-core returned {status}: {body}"))
    }

    /// `GET /logs/recent` (`embarch-core` spec.md, `embarch-ui` decision 7)
    /// — the tail of Core's own current daily log file.
    /// Reuses `status_timeout`: a pure local-file read on Core's side, no
    /// hardware touched. `embarch-ui`'s own Debug tab is the first caller —
    /// never a direct filesystem read of Core's logfile, since Core can run
    /// on a different machine (the whole reason `embarch-topology` exists).
    pub async fn logs_recent(&self, tail: usize) -> Result<Vec<String>> {
        let url = format!("{}/logs/recent", self.base_url().await?);
        let request = self.client.get(url).query(&[("tail", tail.to_string())]);
        let response: LogsRecentResponse = self.send(request, self.status_timeout).await?;
        Ok(response.lines)
    }

    pub async fn serial_log(
        &self,
        port: &str,
        baud: u32,
        duration_ms: u64,
    ) -> Result<SerialLogResponse> {
        let url = format!("{}/serial-log", self.base_url().await?);
        let request = self
            .client
            .get(url)
            .query(&[("port", port), ("baud", &baud.to_string()), ("duration_ms", &duration_ms.to_string())]);
        self.send(request, self.serial_timeout).await
    }

    /// Resolve a Zephyr SoC name to a probe-rs chip target string via
    /// Core's `POST /resolve-chip` (`embarch-core` decision 8) —
    /// used by a `discovery = "zephyr-west"` project's per-call target
    /// resolution (`resolve.rs`, decision 12), since Core
    /// owns the one copy of this mapping. Reuses `status_timeout`: this is
    /// as quick a call as `/status`, no hardware touched on either end.
    pub async fn resolve_chip(&self, soc: &str) -> Result<String> {
        let url = format!("{}/resolve-chip", self.base_url().await?);
        let body = ResolveChipRequest { soc };
        let resp: ResolveChipResponse = self
            .send(self.client.post(url).json(&body), self.status_timeout)
            .await?;
        Ok(resp.chip)
    }

    /// Submit a `Study` for Core to run against whatever DUT is connected
    /// through its one dev-bench serial link (this repo's `spec.md` —
    /// no `project` param, unlike `build`/`flash`, since a study isn't
    /// tied to one of this file's configured projects). Async: a `200`
    /// means Core accepted the study and started it, not that it finished
    /// — poll `get_study_status` for progress.
    ///
    /// Callers must have already recomputed `study.steps_crc` via
    /// `embarch_study_designer::steps_crc` before calling this — this
    /// method sends `study` exactly as given, it does not recompute
    /// anything itself.
    /// Submits a `Study`, **after** confirming Core agrees about
    /// `embarch-study-designer`'s host type schema
    /// (`embarch-study-designer` decision 12 and its
    /// 2026-08-25 amendment).
    ///
    /// The check lives here rather than at each caller because both the CLI
    /// and the MCP path submit through this one method, and a drift detector
    /// that only one of them runs is not a detector. `GET /status` is
    /// already this hop's connection-establishment check, so this is one
    /// extra cheap read immediately before the submit rather than a separate
    /// handshake.
    ///
    /// **A mismatch detector, not a negotiator** — there is no fallback to
    /// an older shape, matching the suite's standing posture. A Core serving
    /// no version at all is reported as a mismatch too, not waved through:
    /// it is a Core built before this field existed, which is precisely the
    /// drift the field was added to catch.
    ///
    /// `run` carries the two things that deliberately cannot ride inside the
    /// `Study` body — see [`StudyRunOptions`]. Passing
    /// `&StudyRunOptions::default()` is the pre-item-2 behavior exactly, and
    /// produces a byte-identical request.
    pub async fn post_study(
        &self,
        study: &Study,
        run: &StudyRunOptions,
    ) -> Result<PostStudyResponse> {
        let core_version = self.status().await?.study_designer_schema_version;
        if core_version != Some(embarch_study_designer::HOST_TYPE_SCHEMA_VERSION) {
            return Err(anyhow::Error::new(SchemaVersionMismatch {
                api_version: embarch_study_designer::HOST_TYPE_SCHEMA_VERSION,
                core_version,
            }));
        }

        let url = format!("{}/study{}", self.base_url().await?, run.query_suffix());
        let response = self
            .dispatch(self.client.post(url).json(study), Some(self.study_timeout))
            .await?;

        let status = response.status();
        if status.is_success() {
            return response
                .json::<PostStudyResponse>()
                .await
                .context("failed to parse embarch-core's response as JSON");
        }

        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<no response body>".to_string());

        if status == reqwest::StatusCode::CONFLICT {
            return match serde_json::from_str::<StudyConflictBody>(&body) {
                Ok(conflict) => Err(anyhow::Error::new(StudyConflictError {
                    study_id: conflict.study_id,
                })),
                Err(_) => Err(anyhow!(
                    "embarch-core returned 409 Conflict (a study is already in-flight), \
                     but its response body didn't name a study_id: {body}"
                )),
            };
        }

        Err(anyhow!(Self::format_study_error(status, &body)))
    }

    /// Poll a submitted study's status via `GET /study/{study_id}`.
    pub async fn get_study_status(&self, study_id: &str) -> Result<StudyStatusResponse> {
        let url = format!("{}/study/{study_id}", self.base_url().await?);
        let response = self
            .dispatch(self.client.get(url), Some(self.study_timeout))
            .await?;

        let status = response.status();
        if status.is_success() {
            return response
                .json::<StudyStatusResponse>()
                .await
                .context("failed to parse embarch-core's response as JSON");
        }

        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<no response body>".to_string());

        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(anyhow!(
                "unknown study_id '{study_id}': embarch-core has no record of it"
            ));
        }

        Err(anyhow!(Self::format_study_error(status, &body)))
    }

    /// Shared by every "fetch a study's captured bytes" call: the three
    /// fixed-channel aliases (`get_study_power_data` and friends) and the
    /// parameterised [`CoreClient::get_study_stream`] they are aliases of.
    /// All are `GET /study/{study_id}/<endpoint>` returning a raw body,
    /// differing only in the endpoint and in what a `404` means there.
    ///
    /// Deliberately **not** a "looks like CSV" branch anywhere: what a tap's
    /// bytes mean is its declared `StreamEncoding` and nothing else
    /// (`embarch-study-designer` decision 39), and Core has
    /// already applied that declaration by the time these bytes are served.
    async fn get_study_csv(&self, endpoint: &str, study_id: &str, not_found: &str) -> Result<Bytes> {
        let url = format!("{}/study/{study_id}/{endpoint}", self.base_url().await?);
        let response = self
            .dispatch(self.client.get(url), Some(self.study_timeout))
            .await?;

        let status = response.status();
        if status.is_success() {
            return response
                .bytes()
                .await
                .context("failed to read embarch-core's response body");
        }

        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(anyhow!("{not_found} (study_id: {study_id})"));
        }

        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<no response body>".to_string());
        Err(anyhow!(Self::format_study_error(status, &body)))
    }

    /// `GET /study/{study_id}/stream/{name}` (`embarch-core` decision 30)
    /// — one declared stream tap's capture, as bytes.
    ///
    /// **This replaced three fixed-channel calls** —
    /// `get_study_power_data`/`get_study_waveform_data`/`get_study_gatt_data`
    /// over `/power-data`, `/waveform-data` and `/gatt-data` — which were kept
    /// as aliases for one release and are now retired. They could not report a
    /// truncated capture, which is why [`CoreClient::study_streams`] exists;
    /// call that to learn a study's tap names rather than guessing one.
    ///
    /// `raw` picks the byte-for-byte `.bin` over the tap's rendered file.
    /// Rendered is the default *when the tap's declared `StreamEncoding` has
    /// a rendering*; a `Raw` or `OutpostTrace` tap has none and serves its
    /// raw bytes either way. Nothing here inspects the bytes to decide —
    /// Core resolved the tap's declared encoding through the study's own
    /// `streams/index.json` before serving anything.
    ///
    /// A `404` covers two expected outcomes and says which: the study
    /// declared no tap by that name (Core's body lists the ones it did), or
    /// that tap captured nothing. Use
    /// [`CoreClient::get_study_status`]'s `result.streams` to see what a
    /// completed study actually captured rather than guessing a name.
    pub async fn get_study_stream(&self, study_id: &str, name: &str, raw: bool) -> Result<Bytes> {
        let endpoint = if raw {
            format!("stream/{}?raw=1", urlencode(name))
        } else {
            format!("stream/{}", urlencode(name))
        };
        self.get_study_csv(
            &endpoint,
            study_id,
            &format!("no capture served for stream tap '{name}'"),
        )
        .await
    }

    /// `POST /signals` — declares (or re-declares) where a named DUT signal
    /// currently goes (`embarch-topology` decision 18's
    /// 2026-08-25 amendment).
    ///
    /// Idempotent by name, and that overwrite **is** the migration path the
    /// decision promises: moving the outpost from a `Direct` route onto
    /// dev-bench pins is one call, and no saved `Study` changes, because a
    /// study names the signal and never the carrier.
    ///
    /// Goes over HTTP rather than calling
    /// `embarch_topology::hardware::declare_signal` in-process for the same
    /// reason enrollment does: Core owns writes to that storage, and a write
    /// from elsewhere would bypass its `hw_lock` (`embarch-topology`
    /// decision 14) — and on this suite's real primary deployment a plain-user
    /// process cannot write the file at all.
    ///
    /// Reuses `status_timeout`: an enrollment-file write on Core's side, no
    /// hardware touched.
    pub async fn declare_signal(&self, link: &SignalLink) -> Result<()> {
        let url = format!("{}/signals", self.base_url().await?);
        self.send_no_content(self.client.post(url).json(link), self.status_timeout)
            .await
    }

    /// `GET /signals` — every declared signal link.
    ///
    /// An empty list is the normal starting state, not a failure: nothing has
    /// been wired yet, and a wire between two headers is invisible to software
    /// until someone says it is there.
    pub async fn list_signals(&self) -> Result<Vec<SignalLink>> {
        let url = format!("{}/signals", self.base_url().await?);
        self.send(self.client.get(url), self.status_timeout).await
    }

    /// `DELETE /signals/{name}` — un-declares a signal.
    ///
    /// `Ok(false)` when nothing was declared under that name, mirroring
    /// `embarch_topology::hardware::remove_signal`'s own distinction rather
    /// than flattening Core's `404` into an error: a caller retracting a row
    /// it thought existed wants to learn it did not, not to handle an error
    /// string.
    pub async fn remove_signal(&self, name: &str) -> Result<bool> {
        let url = format!("{}/signals/{}", self.base_url().await?, urlencode(name));
        let response = self
            .dispatch(self.client.delete(url), Some(self.status_timeout))
            .await?;

        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }
        if status.is_success() {
            return Ok(true);
        }
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<no response body>".to_string());
        Err(anyhow!("embarch-core returned {status}: {body}"))
    }

    /// `POST /dev-bench/link` — declares dev-bench's runtime-link USB
    /// serial, by the bridge's own USB serial number and/or which interface
    /// of it (`embarch-core`'s `set_dev_bench_link_handler`). At least one of
    /// `serial`/`interface` must be given — checked here rather than making
    /// a doomed round trip, since Core's own 400 for neither says exactly
    /// this.
    ///
    /// dev-bench must already be enrolled via [`CoreClient::enroll_probe`]
    /// first — this only ever amends that existing row, same posture
    /// [`CoreClient::declare_signal`] documents for its own write. Reuses
    /// `status_timeout`: a plain enrollment-file write on Core's side, no
    /// hardware touched.
    pub async fn set_dev_bench_link(&self, serial: Option<&str>, interface: Option<u8>) -> Result<()> {
        if serial.is_none() && interface.is_none() {
            return Err(anyhow!(
                "set_dev_bench_link needs at least one of serial or interface"
            ));
        }
        let url = format!("{}/dev-bench/link", self.base_url().await?);
        let body = SetDevBenchLinkRequest {
            serial: serial.map(str::to_string),
            interface,
        };
        self.send_no_content(self.client.post(url).json(&body), self.status_timeout)
            .await
    }

    /// `GET /serial-ports` — every USB serial port **Core's** machine
    /// currently enumerates, unnarrowed.
    ///
    /// What a human picks a `Route::Direct` signal's carrier from
    /// (`embarch-ui` decision 10, routing half). Not
    /// [`CoreClient::dev_bench_port`] with the filter off: that answers "which
    /// port is dev-bench's link" and VID-gates to do it, while a `Direct`
    /// route's USB-UART bridge is a wire's carrier and can carry any VID.
    ///
    /// An empty list is a success — nothing plugged in is a real answer.
    /// Reuses `status_timeout`: Core only reads USB descriptors the OS already
    /// enumerated, opening nothing.
    pub async fn list_serial_ports(&self) -> Result<Vec<SerialPortResponse>> {
        let url = format!("{}/serial-ports", self.base_url().await?);
        self.send(self.client.get(url), self.status_timeout).await
    }

    /// `GET /study/{study_id}/streams` — what a study's taps captured, and
    /// why a trace has no names when it has none.
    ///
    /// `Ok(None)` for a study with no `streams/` directory at all: one that
    /// predates it, or one that never got far enough to write it. That is an
    /// expected state rather than a Core failure, same posture
    /// [`CoreClient::dev_bench_port`] takes for "not plugged in".
    ///
    /// This is the **only** place over HTTP where an unnamed outpost trace is
    /// distinguishable from a named one: `GET /study/{id}`'s `StreamRef` has
    /// no room for the reason and `GET /study/{id}/stream/{name}` serves the
    /// rendered CSV either way. A caller that renders a trace without reading
    /// this is capable of presenting numeric thread pointers as resolved
    /// names, which is the exact defect the manifest check exists to prevent.
    pub async fn study_streams(&self, study_id: &str) -> Result<Option<StudyStreamIndex>> {
        let url = format!("{}/study/{}/streams", self.base_url().await?, urlencode(study_id));
        let response = self
            .dispatch(self.client.get(url), Some(self.status_timeout))
            .await?;

        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status.is_success() {
            return response
                .json::<StudyStreamIndex>()
                .await
                .map(Some)
                .context("failed to parse embarch-core's response as JSON");
        }
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<no response body>".to_string());
        Err(anyhow!("embarch-core returned {status}: {body}"))
    }

    /// `GET /study/{study_id}/steps` — every step in a completed study's
    /// `events.json`, with Core's own per-step arrival stamps.
    ///
    /// `Ok(None)` for a study with no `events.json`: one that never finished,
    /// or one Core's retention sweep has since removed. An expected state, not
    /// an error — same posture as [`CoreClient::study_streams`].
    ///
    /// Reuses `status_timeout`: this is one small file read, not a capture.
    pub async fn study_steps(&self, study_id: &str) -> Result<Option<StudySteps>> {
        let url = format!("{}/study/{}/steps", self.base_url().await?, urlencode(study_id));
        let response = self
            .dispatch(self.client.get(url), Some(self.status_timeout))
            .await?;

        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status.is_success() {
            return response
                .json::<StudySteps>()
                .await
                .map(Some)
                .context("failed to parse embarch-core's response as JSON");
        }
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<no response body>".to_string());
        Err(anyhow!("embarch-core returned {status}: {body}"))
    }

    /// `GET /dev-bench/hello` (`embarch-core` spec.md) — runs the
    /// `Hello`/`HelloAck` handshake on its own and reports what the bench
    /// currently flashed actually says it is. No `Study` is involved and no
    /// study lock is taken beyond Core's own refusal while one is in flight.
    ///
    /// This is the only version string in the suite that is genuinely read
    /// back off the thing it describes, which is why `run_study`'s pre-flight
    /// check uses it rather than deriving the bench's version from a local
    /// checkout the way `embarch-umbrella`'s doctor check 13 has to.
    ///
    /// Reuses `serial_timeout`, not `status_timeout`: unlike every other
    /// `status_timeout` caller in this file, Core does not just read a local
    /// file or an OS-cached USB descriptor before it can answer here — it
    /// opens the bench's serial link, runs the `Hello`/`HelloAck` handshake,
    /// and only then reads the boot log the bench flushes after that ack
    /// (`embarch-core` decision 37), before closing the link. That is link
    /// setup plus a live exchange with the board, the same shape of cost
    /// `serial_log` budgets for on the same physical link, so this route
    /// reuses that budget rather than `status_timeout`'s "no hardware"
    /// justification, which does not hold here. No handshake duration has
    /// been measured on any bench, so 15 s is **assumed**, not measured —
    /// carried over from `serial_log` rather than sized for this route in
    /// particular. A timed authenticated `curl` of this endpoint on the
    /// primary bench would size both at once.
    ///
    /// Goes through [`CoreClient::dispatch`] directly rather than `send`,
    /// because this route gives two non-2xx statuses two entirely different
    /// meanings that a caller needs to tell apart: `409` ([`DevBenchBusyError`])
    /// means a study is already using the link and this call was refused
    /// rather than racing it, `502` ([`DevBenchHandshakeError`]) means the
    /// handshake itself was attempted and failed. Collapsing both into one
    /// generic "non-2xx" error (as `send`'s default funnel does) is exactly
    /// what would send an operator to the wrong place.
    pub async fn dev_bench_hello(&self) -> Result<HelloAckResponse> {
        let url = format!("{}/dev-bench/hello", self.base_url().await?);
        let response = self.dispatch(self.client.get(url), Some(self.serial_timeout)).await?;

        let status = response.status();
        if status.is_success() {
            return response
                .json::<HelloAckResponse>()
                .await
                .context("failed to parse embarch-core's response as JSON");
        }

        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<no response body>".to_string());

        if status == reqwest::StatusCode::CONFLICT {
            return Err(anyhow::Error::new(DevBenchBusyError { message: body }));
        }
        if status == reqwest::StatusCode::BAD_GATEWAY {
            return Err(anyhow::Error::new(DevBenchHandshakeError { message: body }));
        }
        Err(anyhow!("embarch-core returned {status}: {body}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The mirror's contract, written out.**
    ///
    /// [`SignalLink`] is a hand-maintained mirror of
    /// `embarch_topology::hardware::SignalLink`, and no crate in the suite can
    /// see both: the real type is behind that crate's `hardware` feature,
    /// which is what pulls in `probe-rs`/`serialport`, and this crate never
    /// links those. So the coupling is pinned from each side against the same
    /// literal instead — `embarch-core`'s
    /// `the_signal_link_wire_shape_is_what_clients_send` asserts the other
    /// half against this exact string.
    ///
    /// If you change this literal, change that test too; a silent drift here
    /// is a `POST /signals` that fails only against a live Core.
    const SIGNAL_LINK_JSON: &str = concat!(
        r#"{"name":"outpost","origin_role":"dut","direction":"dut-to-host","#,
        r#""route":{"kind":"direct","port_serial":"ABC123"}}"#
    );

    fn outpost_signal() -> SignalLink {
        SignalLink {
            name: "outpost".to_string(),
            origin_role: "dut".to_string(),
            direction: SignalDirection::DutToHost,
            route: SignalRoute::Direct { port_serial: "ABC123".to_string() },
        }
    }

    #[test]
    fn a_declared_signal_serializes_to_the_shape_core_parses() {
        assert_eq!(serde_json::to_string(&outpost_signal()).unwrap(), SIGNAL_LINK_JSON);
        assert_eq!(
            serde_json::from_str::<SignalLink>(SIGNAL_LINK_JSON).unwrap(),
            outpost_signal()
        );
    }

    /// [`AlertResponse`]'s half of the same mirror contract
    /// [`SIGNAL_LINK_JSON`] documents. `embarch-core`'s own
    /// `alert_round_trips_against_the_client_s_pinned_shape` (`src/api.rs`,
    /// `tasks/core/024`) pins the same literal from the other side; if the
    /// two ever disagree, that disagreement — not just a red test here — is
    /// the finding.
    const ALERT_RESPONSE_JSON: &str = concat!(
        r#"{"id":"18f3a2-4242","occurred_at_utc_ms":1725000000000,"role":"dut","#,
        r#""probe_serial":"ABC123","chip":"nrf54l15","recorded_hardware_id":"AAAA","#,
        r#""live_hardware_id":"BBBB","reason":"hardware id mismatch"}"#
    );

    fn sample_alert() -> AlertResponse {
        AlertResponse {
            id: "18f3a2-4242".to_string(),
            occurred_at_utc_ms: 1725000000000,
            role: "dut".to_string(),
            probe_serial: "ABC123".to_string(),
            chip: "nrf54l15".to_string(),
            recorded_hardware_id: "AAAA".to_string(),
            live_hardware_id: Some("BBBB".to_string()),
            reason: "hardware id mismatch".to_string(),
        }
    }

    #[test]
    fn an_alert_round_trips_against_the_pinned_shape() {
        assert_eq!(serde_json::to_string(&sample_alert()).unwrap(), ALERT_RESPONSE_JSON);
        assert_eq!(
            serde_json::from_str::<AlertResponse>(ALERT_RESPONSE_JSON).unwrap(),
            sample_alert()
        );
    }

    /// [`EnrolledBoardResponse`]'s half of the same mirror contract, pinning
    /// `link_port_interface` in particular (`embarch-topology` decision 20)
    /// — the field this task exists to stop the mirror from dropping.
    /// `embarch-core`'s own
    /// `enrolled_board_round_trips_against_the_client_s_pinned_shape`
    /// (`src/api.rs`, `tasks/core/024`) pins the same literal from the other
    /// side, `link_port_interface` included; if the two ever disagree, that
    /// disagreement — not just a red test here — is the finding.
    const ENROLLED_BOARD_RESPONSE_JSON: &str = concat!(
        r#"{"probe_serial":"ABC123","role":"dev-bench","chip":"nrf54l15","#,
        r#""hardware_id":"AAAA","confirmed_at_utc_ms":1725000000000,"#,
        r#""link_port_serial":"D607104","link_port_interface":2}"#
    );

    fn sample_enrolled_board() -> EnrolledBoardResponse {
        EnrolledBoardResponse {
            probe_serial: "ABC123".to_string(),
            role: "dev-bench".to_string(),
            chip: "nrf54l15".to_string(),
            hardware_id: "AAAA".to_string(),
            confirmed_at_utc_ms: 1725000000000,
            link_port_serial: Some("D607104".to_string()),
            link_port_interface: Some(2),
        }
    }

    #[test]
    fn an_enrolled_board_round_trips_against_the_pinned_shape() {
        assert_eq!(
            serde_json::to_string(&sample_enrolled_board()).unwrap(),
            ENROLLED_BOARD_RESPONSE_JSON
        );
        assert_eq!(
            serde_json::from_str::<EnrolledBoardResponse>(ENROLLED_BOARD_RESPONSE_JSON).unwrap(),
            sample_enrolled_board()
        );
    }

    /// [`ValidateResponse`]'s wire shape (`embarch-core/src/api.rs`'s
    /// `ValidateOkResponse`, flat and top-level — not
    /// `embarch_topology::hardware::Validation`'s nested `{ board, .. }`).
    /// Pins that `validated_at_utc_ms` (this call's own live check,
    /// `embarch-topology` decision 26 / `embarch-core` decision 50) parses
    /// distinctly from `confirmed_at_utc_ms` (enrolment time) rather than
    /// one silently shadowing the other.
    #[test]
    fn a_validate_response_parses_both_distinct_timestamps() {
        let json = concat!(
            r#"{"ok":true,"role":"dut","probe_serial":"ABC123","chip":"nrf54l15","#,
            r#""hardware_id":"AAAA","confirmed_at_utc_ms":1725000000000,"#,
            r#""validated_at_utc_ms":1726000000000}"#
        );
        let resp: ValidateResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.confirmed_at_utc_ms, 1725000000000);
        assert_eq!(resp.validated_at_utc_ms, Some(1726000000000));
    }

    /// An older Core that predates `validated_at_utc_ms` (before
    /// `tasks/core/026`) still parses — `#[serde(default)]` to `None`,
    /// mirroring `an_older_core_body_missing_link_port_interface_still_parses`.
    /// `None` must never be read as "validated at 1970".
    #[test]
    fn an_older_core_validate_body_missing_validated_at_still_parses() {
        let json = concat!(
            r#"{"ok":true,"role":"dut","probe_serial":"ABC123","chip":"nrf54l15","#,
            r#""hardware_id":"AAAA","confirmed_at_utc_ms":1725000000000}"#
        );
        let resp: ValidateResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.confirmed_at_utc_ms, 1725000000000);
        assert_eq!(resp.validated_at_utc_ms, None);
    }

    /// `embarch-core` decision 59: `kind: "not_attached"` and
    /// `kind: "mismatch"` are opposite conditions, and this task exists
    /// because a wrapper conflated them under one "topology mismatch" lead.
    /// Constructs both shapes and asserts the two `Display`ed leads differ —
    /// and that `is_not_attached()` reads the field, not `reason`'s wording.
    #[test]
    fn not_attached_and_mismatch_render_distinct_leads() {
        let not_attached = TopologyMismatchError {
            role: "dev-bench".to_string(),
            probe_serial: "001057729826".to_string(),
            chip: "nRF54L15".to_string(),
            recorded_hardware_id: "6fcddc36cb781b71".to_string(),
            live_hardware_id: None,
            reason: "probe '001057729826' enrolled as role 'dev-bench' is not currently \
                      attached"
                .to_string(),
            kind: "not_attached".to_string(),
            fix_it_url: None,
        };
        let mismatch = TopologyMismatchError {
            role: "dev-bench".to_string(),
            probe_serial: "001057729826".to_string(),
            chip: "nRF54L15".to_string(),
            recorded_hardware_id: "6fcddc36cb781b71".to_string(),
            live_hardware_id: Some("aaaaaaaaaaaaaaaa".to_string()),
            reason: "live hardware_id disagrees with the recorded one".to_string(),
            kind: "mismatch".to_string(),
            fix_it_url: Some("http://127.0.0.1:4890/#topology".to_string()),
        };

        assert!(not_attached.is_not_attached());
        assert!(!mismatch.is_not_attached());

        let not_attached_text = not_attached.to_string();
        let mismatch_text = mismatch.to_string();
        assert_ne!(not_attached_text, mismatch_text);
        assert!(
            !not_attached_text.starts_with("topology mismatch"),
            "not_attached must not lead with 'topology mismatch': {not_attached_text}"
        );
        assert!(mismatch_text.starts_with("topology mismatch"), "{mismatch_text}");
        assert!(
            !not_attached_text.contains("fix it at"),
            "not_attached must not offer a fix_it_url: {not_attached_text}"
        );
        assert!(mismatch_text.contains("fix it at"), "{mismatch_text}");
    }

    /// A `409`/`503` body without a `kind` field (a Core older than decision
    /// 59) defaults to `"mismatch"` — the only condition such a Core ever
    /// sent this shape for — and `fix_it_url` still parses as required.
    #[test]
    fn an_older_core_mismatch_body_missing_kind_defaults_to_mismatch() {
        let json = concat!(
            r#"{"role":"dev-bench","probe_serial":"ABC123","chip":"nrf54l15","#,
            r#""recorded_hardware_id":"AAAA","live_hardware_id":"BBBB","#,
            r#""reason":"live hardware_id disagrees","#,
            r#""fix_it_url":"http://127.0.0.1:4890/#topology"}"#
        );
        let body: TopologyMismatchBody = serde_json::from_str(json).unwrap();
        assert_eq!(body.kind, "mismatch");
        assert_eq!(body.fix_it_url, Some("http://127.0.0.1:4890/#topology".to_string()));
    }

    /// `embarch-core` decision 59's `"not_attached"` arm: `fix_it_url` is
    /// `null` and `kind` names the condition explicitly.
    #[test]
    fn a_not_attached_body_has_no_fix_it_url() {
        let json = concat!(
            r#"{"role":"dev-bench","probe_serial":"ABC123","chip":"nrf54l15","#,
            r#""recorded_hardware_id":"AAAA","live_hardware_id":null,"#,
            r#""reason":"probe is not currently attached","kind":"not_attached","#,
            r#""fix_it_url":null}"#
        );
        let body: TopologyMismatchBody = serde_json::from_str(json).unwrap();
        assert_eq!(body.kind, "not_attached");
        assert_eq!(body.fix_it_url, None);
    }

    /// An older Core that predates `link_port_interface` (and, in
    /// principle, `link_port_serial`) still parses — both fields
    /// `#[serde(default)]` to `None`.
    #[test]
    fn an_older_core_body_missing_link_port_interface_still_parses() {
        let json = concat!(
            r#"{"probe_serial":"ABC123","role":"dev-bench","chip":"nrf54l15","#,
            r#""hardware_id":"AAAA","confirmed_at_utc_ms":1725000000000}"#
        );
        let board: EnrolledBoardResponse = serde_json::from_str(json).unwrap();
        assert_eq!(board.link_port_serial, None);
        assert_eq!(board.link_port_interface, None);
    }

    /// The other route variant, whose tag is the one a `rename_all` could
    /// plausibly get wrong (`via-dev-bench`, not `viaDevBench` or
    /// `via_dev_bench`).
    #[test]
    fn the_via_dev_bench_route_keeps_its_kebab_tag() {
        let link = SignalLink {
            name: "outpost".to_string(),
            origin_role: "dut".to_string(),
            direction: SignalDirection::DutToHost,
            route: SignalRoute::ViaDevBench { rx_pin: "P0.04".to_string(), tx_pin: "P0.05".to_string() },
        };
        let json = serde_json::to_string(&link).unwrap();
        assert!(json.contains(r#""kind":"via-dev-bench""#), "{json}");
        assert_eq!(serde_json::from_str::<SignalLink>(&json).unwrap(), link);
    }

    /// [`HelloAckResponse`]'s half of the same mirror contract
    /// [`SIGNAL_LINK_JSON`] documents — pinned against `embarch-core`'s own
    /// `study::HelloAckInfo` field-for-field, `self_reported_hardware_id`,
    /// `link_identity` and `probe_hardware_id` included. The Core-side
    /// counterpart test that pins `HelloAckInfo` against this exact string
    /// does not exist yet — this only pins the client's own read of it.
    const HELLO_ACK_RESPONSE_JSON: &str = concat!(
        r#"{"schema_version":10,"compatible":true,"firmware_version":"g1a2b3c",""#,
        r#"self_reported_hardware_id":"AAAABBBB","link_identity":"undeclared",""#,
        r#"probe_hardware_id":"BBBBAAAA"}"#
    );

    fn sample_hello_ack() -> HelloAckResponse {
        HelloAckResponse {
            schema_version: 10,
            compatible: true,
            firmware_version: "g1a2b3c".to_string(),
            self_reported_hardware_id: Some("AAAABBBB".to_string()),
            link_identity: Some("undeclared".to_string()),
            probe_hardware_id: Some("BBBBAAAA".to_string()),
        }
    }

    #[test]
    fn a_hello_ack_round_trips_against_the_pinned_shape() {
        assert_eq!(serde_json::to_string(&sample_hello_ack()).unwrap(), HELLO_ACK_RESPONSE_JSON);
        assert_eq!(
            serde_json::from_str::<HelloAckResponse>(HELLO_ACK_RESPONSE_JSON).unwrap(),
            sample_hello_ack()
        );
    }

    /// **`link_identity`'s not-a-pass contract, written out.** `"undeclared"`
    /// (today's real answer for every chip, `embarch-core` §3 decision 35)
    /// must round-trip as that exact string, not coerce to any boolean —
    /// this is the regression that would let an absent identity check start
    /// reading as a confirmed one.
    #[test]
    fn link_identity_survives_as_the_literal_string_not_a_bool() {
        let ack = sample_hello_ack();
        assert_eq!(ack.link_identity.as_deref(), Some("undeclared"));
        let json = serde_json::to_string(&ack).unwrap();
        assert!(json.contains(r#""link_identity":"undeclared""#), "{json}");
        assert!(!json.contains(r#""link_identity":true"#));
        assert!(!json.contains(r#""link_identity":false"#));
    }

    /// **`embarch-api` decision 58's whole reason for existing, pinned at
    /// this struct.** A Core older than `embarch-core` decision 47
    /// (`tasks/core/020`, 2026-09-07) serves none of the three identity
    /// fields under these spellings — `self_reported_hardware_id` in
    /// particular predates the rename as plain `hardware_id`, which this
    /// struct does not (and must not) alias. Against that Core's response
    /// body, every field this struct cannot yet see must deserialize to
    /// `None`, not fail the whole response — a bare required `String` here
    /// is exactly the defect `api/036` was refused at the merge for.
    #[test]
    fn an_older_core_missing_all_three_identity_fields_still_deserializes() {
        let json = r#"{"schema_version":9,"compatible":true,"firmware_version":"g0f0f0f"}"#;
        let ack: HelloAckResponse = serde_json::from_str(json).unwrap();
        assert_eq!(ack.schema_version, 9);
        assert!(ack.compatible);
        assert_eq!(ack.firmware_version, "g0f0f0f");
        assert_eq!(ack.self_reported_hardware_id, None);
        assert_eq!(ack.link_identity, None);
        assert_eq!(ack.probe_hardware_id, None);
    }

    /// The refused unit's whole reason for coming back, written as a test:
    /// a response missing `self_reported_hardware_id` **entirely** (not
    /// present as `null`, absent as a key — the shape an older
    /// `embarch-core` actually sends) still deserializes, and the render
    /// leads with the unavailable line rather than a partial pass.
    #[test]
    fn an_incomplete_response_renders_the_unavailable_line_first() {
        let json = concat!(
            r#"{"schema_version":9,"compatible":true,"firmware_version":"g0f0f0f","#,
            r#""link_identity":"undeclared","probe_hardware_id":"BBBBAAAA"}"#
        );
        let ack: HelloAckResponse = serde_json::from_str(json).unwrap();
        assert_eq!(ack.self_reported_hardware_id, None);

        let rendered = render_hello_ack(&ack);
        let unavailable_line = rendered.lines().next().unwrap();
        assert!(unavailable_line.contains("UNAVAILABLE"), "{rendered}");
        assert!(unavailable_line.contains("self_reported_hardware_id"), "{rendered}");
        // The one thing this whole task exists to prevent: an absent field
        // must never render as the board's own "not-reported" answer.
        assert!(!rendered.contains("self_reported_hardware_id: not-reported"), "{rendered}");
        assert!(rendered.contains("this Core did not send this field"), "{rendered}");
        // The present fields may still appear, but only below the leading line.
        let unavailable_pos = rendered.find("UNAVAILABLE").unwrap();
        let probe_pos = rendered.find("probe_hardware_id: BBBBAAAA").unwrap();
        assert!(probe_pos > unavailable_pos, "{rendered}");
    }

    /// The complete-response half of the same contract: every field present
    /// renders verbatim and the note about not-reported/undeclared appears.
    #[test]
    fn a_complete_response_renders_every_field_verbatim() {
        let rendered = render_hello_ack(&sample_hello_ack());
        assert!(rendered.starts_with("Identity cross-check: complete"), "{rendered}");
        assert!(rendered.contains("self_reported_hardware_id: AAAABBBB"), "{rendered}");
        assert!(rendered.contains("probe_hardware_id: BBBBAAAA"), "{rendered}");
        assert!(rendered.contains("link_identity: undeclared"), "{rendered}");
        assert!(rendered.contains("not confirmations"), "{rendered}");
    }

    /// Named and timed are **two** facts, and the pair is why they are two
    /// fields instead of one note.
    #[test]
    fn named_and_timed_are_independent_and_neither_is_read_off_the_note() {
        let entry = |rendered: bool, note: Option<&str>, named, timed| StudyStreamEntry {
            self_excluded: None,
            id: 0,
            name: "outpost".to_string(),
            encoding: StreamEncoding::OutpostTrace,
            rendered,
            note: note.map(str::to_string),
            named,
            timed,
        };

        let both = entry(true, None, Some(true), Some(true));
        assert!(both.is_named() && both.is_timed());

        // The case the old single-flag rule got wrong: a note, and a trace
        // that is genuinely named.
        let untimed = entry(
            true,
            Some("decoded but NOT timed: no arrival stamps were recorded"),
            Some(true),
            Some(false),
        );
        assert!(untimed.is_named(), "an untimed trace is still a named one");
        assert!(!untimed.is_timed());

        let unnamed = entry(true, Some("decoded but NOT named: …"), Some(false), Some(true));
        assert!(!unnamed.is_named());
        assert!(unnamed.is_timed());

        // A Core that predates the fields: fall back to the old conjunction
        // for names, and never claim a time base nobody reported.
        let old = entry(true, None, None, None);
        assert!(old.is_named());
        assert!(!old.is_timed());
        assert!(!entry(true, Some("decoded but NOT named: …"), None, None).is_named());
        assert!(!entry(false, None, None, None).is_named());
    }

    /// The manifest travels because it *sits beside the artifact*, not because
    /// a caller remembered to name it. This is the whole mechanism, so it is
    /// pinned here rather than left to whichever call site happens to be
    /// exercised.
    #[test]
    fn a_manifest_beside_the_artifact_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = dir.path().join("zephyr.hex");
        std::fs::write(&artifact, b"x").unwrap();

        assert_eq!(
            manifest_beside(artifact.to_str().unwrap()),
            None,
            "a build with no outpost must send no manifest"
        );

        let manifest = dir.path().join("outpost-manifest.json");
        std::fs::write(&manifest, b"{}").unwrap();
        assert_eq!(manifest_beside(artifact.to_str().unwrap()), Some(manifest));
    }

    #[test]
    fn a_directory_named_like_the_manifest_is_not_one() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = dir.path().join("zephyr.hex");
        std::fs::write(&artifact, b"x").unwrap();
        std::fs::create_dir(dir.path().join("outpost-manifest.json")).unwrap();

        assert_eq!(manifest_beside(artifact.to_str().unwrap()), None);
    }

    /// A default-options submit must produce the exact URL every caller sent
    /// before these parameters existed. The three old MCP tools and the
    /// `embarch-ui` Study Designer are still on that path, and an alias that
    /// quietly started sending something different is precisely the
    /// mid-flight breakage keeping them as aliases exists to avoid.
    #[test]
    fn default_run_options_change_the_request_not_at_all() {
        assert_eq!(StudyRunOptions::default().query_suffix(), "");
    }

    #[test]
    fn each_run_option_appears_only_when_it_is_actually_set() {
        let allow = StudyRunOptions { allow_version_mismatch: true, ..Default::default() };
        assert_eq!(allow.query_suffix(), "?allow_version_mismatch=1");

        let flashed = StudyRunOptions {
            flashed_firmware_version: Some("g1a2b3c".to_string()),
            ..Default::default()
        };
        assert_eq!(flashed.query_suffix(), "?flashed_firmware_version=g1a2b3c");

        let both = StudyRunOptions {
            allow_version_mismatch: true,
            flashed_firmware_version: Some("g1a2b3c-dirty".to_string()),
        };
        assert_eq!(
            both.query_suffix(),
            "?allow_version_mismatch=1&flashed_firmware_version=g1a2b3c-dirty"
        );
    }

    /// A version string is free-form: it comes from a project-declared
    /// command, not from a fixed `git describe` this crate controls. A space
    /// or an `&` in one must not become a second query parameter.
    #[test]
    fn a_version_string_cannot_smuggle_a_second_query_parameter() {
        let sneaky = StudyRunOptions {
            flashed_firmware_version: Some("v1 &allow_version_mismatch=1".to_string()),
            ..Default::default()
        };
        let suffix = sneaky.query_suffix();
        assert_eq!(suffix, "?flashed_firmware_version=v1%20%26allow_version_mismatch%3D1");
        assert!(!suffix.contains("&allow_version_mismatch=1"));
    }

    #[test]
    fn urlencode_leaves_the_unreserved_set_alone() {
        assert_eq!(urlencode("g1a2b3c-dirty_x.y~z"), "g1a2b3c-dirty_x.y~z");
        assert_eq!(urlencode("a/b"), "a%2Fb");
    }
}
