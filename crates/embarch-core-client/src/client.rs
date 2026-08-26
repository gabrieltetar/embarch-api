use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use embarch_study_designer::{Study, StudyResult};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OnceCell;

use crate::CoreConfig;
use embarch_topology::software::{ProbeOutcome, TopologyClass};

/// Where Core's address comes from.
enum Address {
    /// Declared in config, used as-is.
    Declared(String),
    /// `base_url = "auto"` — discovered by probing (config.rs, design.md
    /// §3.11). Deliberately not resolved at construction time: the startup
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
    /// (`embarch-study-designer/design.md` §3 decision 12 and its
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
/// type schema (`embarch-study-designer/design.md` §3 decision 12).
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
                 `GET /status` carrying one (design.md §3 decision 12's 2026-08-25 amendment). \
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
}

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

/// `embarch-core/design.md` §3 decision 22's `POST /probes/enroll` — the
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
    /// present (`embarch-core/design.md` §3 decision 22's own doc comment;
    /// `embarch-topology/design.md` §3 decision 15) — omitted, Core falls
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
    pub hardware_id: String,
    pub confirmed_at_utc_ms: u64,
}

/// `embarch-core/design.md` §3 decision 28's `POST /validate` — an
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
    pub hardware_id: String,
    pub confirmed_at_utc_ms: u64,
}

/// `POST /validate`'s `409 Conflict` body — the enrolled board's live
/// identity no longer matches what's recorded.
#[derive(Debug, Deserialize)]
struct TopologyMismatchBody {
    role: String,
    probe_serial: String,
    chip: String,
    recorded_hardware_id: String,
    live_hardware_id: Option<String>,
    reason: String,
    fix_it_url: String,
}

/// Distinct error for `POST /validate`'s `409 Conflict` — kept as its own
/// downcastable type (`StudyConflictError`'s own precedent above) so a
/// caller that wants to branch on "this specifically is a stale identity,
/// not some other failure" can `e.downcast_ref::<TopologyMismatchError>()`
/// for it, including `fix_it_url` to relay onward (never auto-opened here —
/// `embarch-topology/design.md` §3 decision 12's "opening the UI is the
/// caller's job," and this crate's own posture is to relay it as text, same
/// as `embarch-topology validate`'s own CLI never opening a browser).
#[derive(Debug)]
pub struct TopologyMismatchError {
    pub role: String,
    pub probe_serial: String,
    pub chip: String,
    pub recorded_hardware_id: String,
    pub live_hardware_id: Option<String>,
    pub reason: String,
    pub fix_it_url: String,
}

impl std::fmt::Display for TopologyMismatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "topology mismatch: {} — fix it at {}", self.reason, self.fix_it_url)
    }
}

impl std::error::Error for TopologyMismatchError {}

/// One entry from `GET /alerts` — mirrors `embarch_topology::hardware::
/// Alert`'s fields without depending on that crate's `hardware` feature
/// (this crate deliberately never links `probe-rs`/`serialport`,
/// `embarch-topology/design.md` §4's own "no hardware knowledge" boundary).
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// One entry from `GET /probes/enrolled` (`embarch-core/design.md` §3
/// decision 25, `link_port_serial` added decision 27) — every currently
/// enrolled board. Added 2026-08-24 for `embarch-ui`'s Dashboard/Topology
/// tabs (`embarch-ui/design.md` §3 decision 5's amendment): reading this
/// over HTTP, rather than `embarch_topology::hardware::list_enrolled()`
/// in-process, is what keeps it correct when Core runs on a different
/// machine than whichever process is asking — the same "never link
/// probe-rs/serialport directly" rule §11 already states for this crate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrolledBoardResponse {
    pub probe_serial: String,
    pub role: String,
    pub chip: String,
    pub hardware_id: String,
    pub confirmed_at_utc_ms: u64,
    #[serde(default)]
    pub link_port_serial: Option<String>,
}

/// `GET /dev-bench/port`'s success body (`embarch-core/design.md` §4/§5) —
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

/// `GET /logs/recent`'s body (`embarch-core/design.md` §4) — plain lines
/// exactly as `tracing_subscriber`'s own formatter wrote them, no
/// server-side structuring/filtering (`embarch-ui/design.md` §3 decision
/// 7's resolution of that open question).
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

/// `POST /study`'s success (200) body — `embarch-doc`'s `embarch-api/design.md`
/// §5: `{ "study_id": "<uuid-string>", "status": "accepted" }`. Only
/// `study_id` is modeled — `run_study`/`run-study` return `{ study_id }`
/// verbatim (per spec) and have no use for `status`, which is always
/// `"accepted"` on a 200 anyway; serde ignores the extra field on
/// deserialize.
#[derive(Debug, Deserialize)]
pub struct PostStudyResponse {
    pub study_id: String,
}

/// The two out-of-band run parameters `POST /study` accepts as **query
/// parameters** (`embarch-core/design.md` §3 decision 31's amendment,
/// `embarch-api/design.md` §3 decision 40).
///
/// Neither can ride inside the `Study` body: `embarch-study-designer/design.md`
/// §3 decision 40 settles that reflash is "a run parameter, not a study
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
    /// can say so (`embarch-core/design.md` §3 decision 31's implementation
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

/// `GET /dev-bench/hello`'s body (`embarch-core/design.md` §4) — the
/// `Hello`/`HelloAck` handshake run on its own, with no `Study` involved.
/// `firmware_version` is what the bench currently running actually reports,
/// which is the only version in this suite that is genuinely read back off
/// the thing it describes.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HelloAckResponse {
    pub schema_version: u32,
    pub compatible: bool,
    pub firmware_version: String,
}

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

/// `GET /study/{study_id}`'s body — `embarch-api/design.md` §5. `status` is
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

/// Core's structured non-2xx error body (`embarch-core/design.md` §3
/// decision 12): `{"code": "...", "message": "...", "cause": "..."}`. Not
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

                // embarch-topology/design.md decisions 2, 3: live, in-process,
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

    async fn base_url(&self) -> Result<&str> {
        Ok(&self.resolved_address().await?.0)
    }

    /// The winning topology class — `Local` for a declared address (no
    /// probing done), otherwise whichever candidate actually answered.
    /// `flash`'s only consumer: a `WslHost`/`Remote` Core can't be assumed
    /// to share a filesystem with this process (`design.md` §9's 2026-08-18
    /// finding — a Session-0-service Core can't reach a `WslHost`'s
    /// `\\wsl.localhost` UNC path at all), so those classes get the
    /// artifact's bytes instead of a path.
    async fn topology_class(&self) -> Result<TopologyClass> {
        Ok(self.resolved_address().await?.1)
    }

    /// Core's error responses are plain-text bodies (axum's IntoResponse for
    /// `(StatusCode, String)`), not JSON — so non-2xx bodies must be read as
    /// text, never parsed as JSON, or Core's actual error message is lost.
    async fn send<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
        timeout: Duration,
    ) -> Result<T> {
        let response = request
            .bearer_auth(&self.token)
            .timeout(timeout)
            .send()
            .await
            .context("request to embarch-core failed")?;

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

    /// Formats a non-2xx `/study/*` error body: Core's new `{code, message,
    /// cause}` shape (`embarch-core/design.md` §3 decision 12) if the body
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
    /// depends on the resolved topology (`design.md` §9's 2026-08-18
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
    /// `base_address` (`embarch-core/design.md` §3 decision 18) is only
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
    /// `probe_serial` (`embarch-core/design.md` §3 decision 9) disambiguates
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

        match self.topology_class().await? {
            TopologyClass::Local => {
                let body = FlashRequest {
                    chip,
                    firmware_path,
                    format,
                    base_address,
                    probe_serial,
                    erase,
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

    /// `POST /probes/enroll` (`embarch-core/design.md` §3 decision 22,
    /// `embarch-api/design.md` §3 decision 34) — records which physical
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

    /// `POST /validate` (`embarch-core/design.md` §3 decision 28) — the
    /// explicit, non-destructive counterpart to the live re-check
    /// `flash`/`reset`/`run_study` already run mid-attach: same underlying
    /// `embarch_topology::hardware::validate_role` call, callable on its own
    /// at any time. Reuses `reset_timeout`, same reasoning as `enroll_probe`
    /// above: one probe attach plus a couple of memory reads, not a
    /// multi-second flash.
    pub async fn validate(&self, role: &str) -> Result<ValidateResponse> {
        let url = format!("{}/validate", self.base_url().await?);
        let response = self
            .client
            .post(url)
            .bearer_auth(&self.token)
            .timeout(self.reset_timeout)
            .json(&ValidateRequest { role })
            .send()
            .await
            .context("request to embarch-core failed")?;

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

        if status == reqwest::StatusCode::CONFLICT {
            return match serde_json::from_str::<TopologyMismatchBody>(&body) {
                Ok(m) => Err(anyhow::Error::new(TopologyMismatchError {
                    role: m.role,
                    probe_serial: m.probe_serial,
                    chip: m.chip,
                    recorded_hardware_id: m.recorded_hardware_id,
                    live_hardware_id: m.live_hardware_id,
                    reason: m.reason,
                    fix_it_url: m.fix_it_url,
                })),
                Err(_) => Err(anyhow!(
                    "embarch-core returned 409 Conflict (a topology mismatch), but its response \
                     body didn't parse as expected: {body}"
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

    /// `GET /alerts` (`embarch-core/design.md` §3 decision 28) — recent
    /// topology-mismatch alerts from Core's durable log
    /// (`embarch_topology::hardware::recent_alerts`). Reuses
    /// `status_timeout`: a pure local-file read on Core's side, no hardware
    /// touched.
    pub async fn alerts(&self, limit: usize) -> Result<Vec<AlertResponse>> {
        let url = format!("{}/alerts", self.base_url().await?);
        let request = self.client.get(url).query(&[("limit", limit.to_string())]);
        self.send(request, self.status_timeout).await
    }

    /// `GET /probes/enrolled` (`embarch-core/design.md` §3 decision 25) —
    /// every currently enrolled board. Reuses `status_timeout`: a pure read
    /// of `embarch-topology`'s own storage on Core's side, no hardware
    /// touched — same posture as `alerts` above.
    pub async fn list_enrolled(&self) -> Result<Vec<EnrolledBoardResponse>> {
        let url = format!("{}/probes/enrolled", self.base_url().await?);
        self.send(self.client.get(url), self.status_timeout).await
    }

    /// `GET /dev-bench/port` (`embarch-core/design.md` §4/§5) — which
    /// serial port `embarch-dev-bench` is on right now, if any. Core's own
    /// `404` for "no port matches" is an expected state (bench unplugged),
    /// not a Core failure, so it's surfaced as `Ok(None)` rather than an
    /// error — a caller that wants to render "not connected" doesn't need
    /// to match on an error string to do it.
    pub async fn dev_bench_port(&self) -> Result<Option<DevBenchPortResponse>> {
        let url = format!("{}/dev-bench/port", self.base_url().await?);
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.token)
            .timeout(self.status_timeout)
            .send()
            .await
            .context("request to embarch-core failed")?;

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

    /// `GET /logs/recent` (`embarch-core/design.md` §4, `embarch-ui/design.md`
    /// §3 decision 7) — the tail of Core's own current daily log file.
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
    /// Core's `POST /resolve-chip` (`embarch-core/design.md` §3 decision 8) —
    /// used by a `discovery = "zephyr-west"` project's per-call target
    /// resolution (`resolve.rs`, `design.md` §3 decision 12), since Core
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
    /// through its one dev-bench serial link (`embarch-api/design.md` §5 —
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
    /// (`embarch-study-designer/design.md` §3 decision 12 and its
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
            .client
            .post(url)
            .bearer_auth(&self.token)
            .timeout(self.study_timeout)
            .json(study)
            .send()
            .await
            .context("request to embarch-core failed")?;

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
            .client
            .get(url)
            .bearer_auth(&self.token)
            .timeout(self.study_timeout)
            .send()
            .await
            .context("request to embarch-core failed")?;

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
    /// (`embarch-study-designer/design.md` §3 decision 35), and Core has
    /// already applied that declaration by the time these bytes are served.
    async fn get_study_csv(&self, endpoint: &str, study_id: &str, not_found: &str) -> Result<Bytes> {
        let url = format!("{}/study/{study_id}/{endpoint}", self.base_url().await?);
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.token)
            .timeout(self.study_timeout)
            .send()
            .await
            .context("request to embarch-core failed")?;

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

    /// `GET /study/{study_id}/power-data` — raw CSV bytes. A `404` is an
    /// expected outcome for many studies (no step declared a
    /// `power_sample`), not an exceptional one, so it's worded as such
    /// rather than as a generic request failure.
    pub async fn get_study_power_data(&self, study_id: &str) -> Result<Bytes> {
        self.get_study_csv(
            "power-data",
            study_id,
            "no power data captured for this study",
        )
        .await
    }

    /// `GET /study/{study_id}/waveform-data` — raw CSV bytes. Same "expected,
    /// not exceptional" `404` posture as `get_study_power_data`: many studies
    /// have no `GattOperation::StreamCapture` step at all.
    pub async fn get_study_waveform_data(&self, study_id: &str) -> Result<Bytes> {
        self.get_study_csv(
            "waveform-data",
            study_id,
            "no waveform data captured for this study",
        )
        .await
    }

    /// `GET /study/{study_id}/gatt-data` — the study's whole GATT transcript
    /// as raw CSV bytes (`embarch-study-designer/design.md` §3 decision 36,
    /// §4.3b): every notification, indication, read, write, subscribe and
    /// connection event, across every step, uncapped.
    ///
    /// Distinct from what `GET /study/{id}` returns inline: that carries each
    /// step's `gatt_activity`, a bounded per-step summary (at most
    /// `MAX_GATT_ACTIVITY_RECORDS` inbound notifications, nothing outbound).
    /// Same "expected, not exceptional" `404` posture as the two above — a
    /// study with no GATT steps captured no transcript.
    pub async fn get_study_gatt_data(&self, study_id: &str) -> Result<Bytes> {
        self.get_study_csv(
            "gatt-data",
            study_id,
            "no GATT transcript captured for this study",
        )
        .await
    }

    /// `GET /study/{study_id}/stream/{name}` (`embarch-core/design.md` §3
    /// decision 30) — one declared stream tap's capture, as bytes. The
    /// parameterised route the three fixed-channel calls above are now
    /// aliases of.
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

    /// `GET /dev-bench/hello` (`embarch-core/design.md` §4) — runs the
    /// `Hello`/`HelloAck` handshake on its own and reports what the bench
    /// currently flashed actually says it is. No `Study` is involved and no
    /// study lock is taken beyond Core's own refusal while one is in flight.
    ///
    /// This is the only version string in the suite that is genuinely read
    /// back off the thing it describes, which is why `run_study`'s pre-flight
    /// check uses it rather than deriving the bench's version from a local
    /// checkout the way `embarch-umbrella`'s doctor check 13 has to.
    ///
    /// Reuses `status_timeout`: like `/status`, this is one short serial
    /// exchange, not a flash.
    pub async fn dev_bench_hello(&self) -> Result<HelloAckResponse> {
        let url = format!("{}/dev-bench/hello", self.base_url().await?);
        self.send(self.client.get(url), self.status_timeout).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
