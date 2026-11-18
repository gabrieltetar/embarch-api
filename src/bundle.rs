//! `bundle`: a finished build, packaged for someone with a debug probe and the
//! vendor's flashing CLI but no toolchain and no checkout.
//!
//! One implementation behind the CLI verb and the MCP tool. It reuses the
//! project's artifact when that is newer than its sources, builds one when it
//! is not, and refuses (writing nothing) when the artifact is still older than
//! the sources afterwards, when the hex lacks an image at the bootloader's or
//! the application slot's address, or when the commit cannot be named.
//!
//! The archive and the hex are read and written with Rust libraries (`zip`,
//! `ihex`): no shell `zip`, no scripting language, so it works on a machine
//! with neither.
//!
//! **Where the partition addresses come from.** The build's own
//! `zephyr/zephyr.dts` (the devicetree the build compiled), never a constant
//! here: the node labelled `mcuboot` is the bootloader and the one labelled
//! `image-0` is the application slot, Zephyr's own labels for them. A
//! partition's address is its `reg` offset plus the unit address of the flash
//! node that holds it, with 32-bit cells.
//!
//! **Where the README's commands come from.** [`FAMILIES`], and only what the
//! suite already records for that chip family (the flash backends in
//! `embarch-core/src/flash_backend.rs` and its decisions). A family it records
//! nothing for gets a line saying so; a command is never written from memory.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use embarch_firmware_build::build::BuildLocks;
use embarch_firmware_build::config::ProjectConfig;
use embarch_firmware_build::resolve::Resolved;

/// The sysbuild-merged image, which sits at the root of the build directory
/// beside the `zephyr/` directory the artifact path points into.
const MERGED_HEX_NAME: &str = "merged.hex";
/// Zephyr's own labels for the bootloader and the application's first slot.
const BOOTLOADER_LABEL: &str = "mcuboot";
const APP_SLOT_LABEL: &str = "image-0";
/// Directories a source scan never descends into.
const SKIPPED_DIRS: [&str; 4] = [".git", "build", "target", "node_modules"];

const README_TEMPLATE: &str = "# {name} at {sha}\n\
\n\
Firmware image `{hex}`, built from commit {sha}{dirty}.\n\
\n\
Segments checked before this was written: {segments}.\n\
\n\
## Flash it\n\
\n\
{commands}\n\
## Console\n\
\n\
{console}\n\
{notes}";

/// What the suite records for one chip family. `None` means the suite records
/// nothing, and the README says so.
struct Family {
    /// Matched against the lower-cased chip name, the way Core matches a
    /// family: by prefix, so a part the suite never heard of is not claimed.
    prefix: &'static str,
    /// Where the suite records it, named in the README.
    source: &'static str,
    /// Program, erasing the ranges the firmware touches. `{hex}` is the file name.
    program: Option<&'static str>,
    reset: Option<&'static str>,
    recover: Option<&'static str>,
}

const FAMILIES: [Family; 1] = [Family {
    prefix: "nrf54l",
    source: "embarch-core's flash backends and its decisions on flashing",
    // What Core's nrfutil backend runs for an erase request (decision 36). It
    // is a construction-time contract nobody has confirmed on a board.
    program: Some(
        "nrfutil device program --firmware {hex} --options chip_erase_mode=ERASE_RANGES_TOUCHED_BY_FIRMWARE",
    ),
    // The reset-and-go sequence Core's J-Link backend scripts (`r`, `g`,
    // `qc`); the suite records no `nrfutil` reset.
    reset: Some(
        "J-Link Commander, with `-device {jlink_device} -if SWD -speed 4000 -autoconnect 1 -NoGui 1 -ExitOnError 1`, \
         running a script of three lines: `r`, `g`, `qc`",
    ),
    // Decision 32 records the vendor's runner with `--erase` as the recovery;
    // decision 36 maps that `--erase` to the range erase in the program line.
    recover: Some("run the program command above again: Core's decisions record the vendor runner's `--erase` as the recovery and map it to that range erase"),
}];

fn family_for(chip: &str) -> Option<&'static Family> {
    let c = chip.to_ascii_lowercase();
    FAMILIES.iter().find(|f| c.starts_with(f.prefix))
}

/// SEGGER's device name for the part, as Core's J-Link backend spells it.
fn jlink_device(chip: &str) -> String {
    let c = chip.to_ascii_lowercase();
    for part in ["nrf54l15", "nrf54l10", "nrf54l05"] {
        if c.starts_with(part) {
            return format!("nRF{}_M33", part[3..].to_ascii_uppercase());
        }
    }
    chip.to_string()
}

/// A partition the build's devicetree defines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    pub label: String,
    pub address: u64,
    pub size: u64,
}

fn parse_cell(s: &str) -> Option<u64> {
    let s = s.trim();
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => s.parse().ok(),
    }
}

/// Partitions out of a compiled `zephyr.dts`: every node carrying a `label`
/// and a `reg` of two cells, placed by the unit address of its nearest
/// addressed ancestor above the `partitions` node.
pub fn parse_partitions(dts: &str) -> Vec<Partition> {
    struct Node {
        header: String,
        label: Option<String>,
        reg: Option<(u64, u64)>,
    }
    let mut stack: Vec<Node> = Vec::new();
    let mut found = Vec::new();
    for line in dts.lines() {
        let line = line.trim();
        if line.ends_with('{') {
            let header = line.trim_end_matches('{').trim();
            let header = header.rsplit(':').next().unwrap_or(header).trim().to_string();
            stack.push(Node { header, label: None, reg: None });
        } else if line.starts_with("};") {
            let Some(node) = stack.pop() else { continue };
            if let (true, Some(label), Some((offset, size))) = (node.header.starts_with("partition@"), node.label, node.reg) {
                let base = stack
                    .iter()
                    .rev()
                    .filter(|n| n.header != "partitions")
                    .find_map(|n| n.header.split_once('@').and_then(|(_, a)| parse_cell(&format!("0x{a}"))))
                    .unwrap_or(0);
                found.push(Partition { label, address: base + offset, size });
            }
        } else if let Some(top) = stack.last_mut() {
            if let Some(v) = line.strip_prefix("label =") {
                top.label = Some(v.trim().trim_end_matches(';').trim().trim_matches('"').to_string());
            } else if let Some(v) = line.strip_prefix("reg =") {
                let cells: Vec<u64> = v
                    .trim()
                    .trim_end_matches(';')
                    .trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .split_whitespace()
                    .filter_map(parse_cell)
                    .collect();
                if let [offset, size] = cells[..] {
                    top.reg = Some((offset, size));
                }
            }
        }
    }
    found
}

/// The contiguous runs of data a hex file holds, as `[start, end)`.
pub fn hex_segments(hex: &str) -> Result<Vec<(u64, u64)>> {
    use ihex::Record;
    let mut base: u64 = 0;
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for record in ihex::Reader::new(hex) {
        match record.context("not a valid Intel hex file")? {
            Record::Data { offset, value } if !value.is_empty() => {
                let start = base + u64::from(offset);
                runs.push((start, start + value.len() as u64));
            }
            Record::ExtendedLinearAddress(a) => base = u64::from(a) << 16,
            Record::ExtendedSegmentAddress(a) => base = u64::from(a) << 4,
            _ => {}
        }
    }
    runs.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (s, e) in runs {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    Ok(merged)
}

fn holds(segments: &[(u64, u64)], address: u64) -> bool {
    segments.iter().any(|&(s, e)| s <= address && address < e)
}

/// Refuses a merged image that does not hold both the bootloader and the
/// application slot, naming which is missing.
pub fn check_segments(segments: &[(u64, u64)], bootloader: &Partition, app: &Partition) -> Result<String> {
    for p in [bootloader, app] {
        if !holds(segments, p.address) {
            bail!(
                "the hex has no data at the {} partition's address 0x{:X} (the build's own partition table puts it there): \
                 it is not a complete merged image",
                p.label,
                p.address
            );
        }
    }
    Ok(format!(
        "{} at 0x{:X} and {} at 0x{:X}",
        bootloader.label, bootloader.address, app.label, app.address
    ))
}

/// The newest modification time under `root`, skipping [`SKIPPED_DIRS`] and
/// `skip` (the build directory, wherever it is).
fn newest_mtime(root: &Path, skip: &Path) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = None;
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                let name = entry.file_name();
                if path.starts_with(skip) || SKIPPED_DIRS.iter().any(|s| name == *s) {
                    continue;
                }
                dirs.push(path);
            } else if let Ok(m) = meta.modified() {
                newest = Some(newest.map_or(m, |n| n.max(m)));
            }
        }
    }
    newest
}

/// Whether `artifact` is older than anything in the source tree. A missing
/// artifact counts as stale.
pub fn artifact_is_stale(artifact: &Path, sources: &Path, build_dir: &Path) -> bool {
    let Ok(built) = std::fs::metadata(artifact).and_then(|m| m.modified()) else {
        return true;
    };
    newest_mtime(sources, build_dir).is_some_and(|newest| newest > built)
}

/// The build directory, and the hex to ship from it: the sysbuild-merged one
/// where the build left it, the artifact itself otherwise.
fn locate_hex(artifact: &Path) -> (PathBuf, PathBuf) {
    let zephyr_dir = artifact.parent().unwrap_or(Path::new(""));
    if zephyr_dir.file_name().is_some_and(|n| n == "zephyr") {
        if let Some(build_dir) = zephyr_dir.parent() {
            let merged = build_dir.join(MERGED_HEX_NAME);
            if merged.is_file() {
                return (build_dir.to_path_buf(), merged);
            }
            return (build_dir.to_path_buf(), artifact.to_path_buf());
        }
    }
    (zephyr_dir.to_path_buf(), artifact.to_path_buf())
}

/// Every `zephyr.dts` the build wrote: the artifact's own directory, then each
/// image directory under the build directory, in name order.
fn dts_files(artifact: &Path, build_dir: &Path) -> Vec<PathBuf> {
    let mut files = vec![artifact.parent().unwrap_or(Path::new("")).join("zephyr.dts")];
    files.push(build_dir.join("zephyr").join("zephyr.dts"));
    if let Ok(entries) = std::fs::read_dir(build_dir) {
        let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        dirs.sort();
        files.extend(dirs.into_iter().map(|d| d.join("zephyr").join("zephyr.dts")));
    }
    let mut seen = Vec::new();
    files.retain(|f| f.is_file() && !seen.contains(f) && {
        seen.push(f.clone());
        true
    });
    files
}

/// The bootloader's and the application slot's partitions, out of the build.
pub fn build_partitions(artifact: &Path, build_dir: &Path) -> Result<(Partition, Partition)> {
    let files = dts_files(artifact, build_dir);
    if files.is_empty() {
        bail!("the build left no zephyr.dts under {}: no partition table to check the hex against", build_dir.display());
    }
    for f in &files {
        let parts = parse_partitions(&std::fs::read_to_string(f).with_context(|| format!("reading {}", f.display()))?);
        let find = |label: &str| parts.iter().find(|p| p.label == label).cloned();
        if let (Some(b), Some(a)) = (find(BOOTLOADER_LABEL), find(APP_SLOT_LABEL)) {
            return Ok((b, a));
        }
    }
    bail!(
        "no zephyr.dts in the build defines both a '{BOOTLOADER_LABEL}' and an '{APP_SLOT_LABEL}' partition: \
         this is not an MCUboot build a bundle can check"
    )
}

/// What a commit is called in the bundle's name, and whether the tree had
/// changes the commit does not hold.
pub fn commit_of(source: &Path) -> Result<(String, bool)> {
    let git = |args: &[&str]| -> Result<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(source)
            .args(args)
            .output()
            .context("failed to run git")?;
        if !out.status.success() {
            bail!("git {} failed in {}: {}", args.join(" "), source.display(), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let sha = git(&["rev-parse", "--short", "HEAD"])?;
    let dirty = !git(&["status", "--porcelain"])?.is_empty();
    Ok((sha, dirty))
}

/// Keeps a project name usable as a file name.
fn safe_name(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' { c } else { '-' }).collect()
}

/// Everything the README and the archive are made from.
pub struct Contents<'a> {
    pub name: &'a str,
    pub sha: &'a str,
    pub dirty: bool,
    pub chip: &'a str,
    pub serial_baud: Option<u32>,
    pub segments: &'a str,
    pub notes: Option<&'a str>,
}

/// The README, rendered from [`README_TEMPLATE`].
pub fn render_readme(c: &Contents<'_>, hex_name: &str) -> String {
    let mut commands = String::new();
    match family_for(c.chip) {
        Some(f) => {
            let fill = |s: &str| s.replace("{hex}", hex_name).replace("{jlink_device}", &jlink_device(c.chip));
            let _ = writeln!(commands, "Chip `{}`. Every command below is what {} records for this family. \
                 The program line's erase mode is a contract Core's decision 36 records as not yet confirmed on a board.\n", c.chip, f.source);
            for (title, text) in [("Program, erasing the ranges the firmware touches", f.program), ("Reset", f.reset), ("Recover", f.recover)] {
                match text {
                    Some(t) if title == "Reset" || title == "Recover" => {
                        let _ = writeln!(commands, "- **{title}:** {}", fill(t));
                    }
                    Some(t) => {
                        let _ = writeln!(commands, "- **{title}:**\n\n  ```\n  {}\n  ```", fill(t));
                    }
                    None => {
                        let _ = writeln!(commands, "- **{title}:** the suite records no command for this.");
                    }
                }
            }
        }
        None => {
            let _ = writeln!(
                commands,
                "The suite records no flashing, reset or recover command for chip `{}`, so none is written here. \
                 Use the vendor's own tool with `{hex_name}`.",
                c.chip
            );
        }
    }
    let console = match c.serial_baud {
        Some(b) => format!("{b} baud, as the project's configuration records it. The suite records no other console setting."),
        None => "The project's configuration records no console setting, so none is written here.".to_string(),
    };
    let notes = c.notes.map(|n| format!("\n## Notes\n\n{}\n", n.trim_end())).unwrap_or_default();
    README_TEMPLATE
        .replace("{name}", c.name)
        .replace("{sha}", c.sha)
        .replace("{hex}", hex_name)
        .replace("{dirty}", if c.dirty { ", with uncommitted changes in the tree" } else { "" })
        .replace("{segments}", c.segments)
        .replace("{commands}", &commands)
        .replace("{console}", &console)
        .replace("{notes}", &notes)
}

/// What was written.
#[derive(Debug)]
pub struct Written {
    pub folder: PathBuf,
    pub hex: PathBuf,
    pub readme: PathBuf,
    pub zip: PathBuf,
}

/// Writes `<name>-<sha>/` and `<name>-<sha>.zip` under `out`.
pub fn write_bundle(out: &Path, stem: &str, hex_bytes: &[u8], readme: &str) -> Result<Written> {
    let folder = out.join(stem);
    std::fs::create_dir_all(&folder).with_context(|| format!("creating {}", folder.display()))?;
    let hex_name = format!("{stem}.hex");
    let hex = folder.join(&hex_name);
    let readme_path = folder.join("README.md");
    std::fs::write(&hex, hex_bytes).with_context(|| format!("writing {}", hex.display()))?;
    std::fs::write(&readme_path, readme).with_context(|| format!("writing {}", readme_path.display()))?;

    let zip_path = out.join(format!("{stem}.zip"));
    let file = std::fs::File::create(&zip_path).with_context(|| format!("creating {}", zip_path.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zip.add_directory(format!("{stem}/"), opts)?;
    for (name, bytes) in [(hex_name.as_str(), hex_bytes), ("README.md", readme.as_bytes())] {
        zip.start_file(format!("{stem}/{name}"), opts)?;
        zip.write_all(bytes)?;
    }
    zip.finish()?;
    Ok(Written { folder, hex, readme: readme_path, zip: zip_path })
}

/// The verb's answer, the way `bootload`'s is: a JSON value and a sentence.
pub struct Report {
    pub success: bool,
    pub value: serde_json::Value,
    pub human: String,
}

fn refuse(reason: String) -> Report {
    Report { success: false, value: serde_json::json!({ "success": false, "error": reason }), human: reason }
}

/// Packages `project`'s build, building first when the artifact is missing or
/// older than its sources.
pub async fn bundle(
    locks: &BuildLocks,
    project: &ProjectConfig,
    resolved: &Resolved,
    notes: Option<&Path>,
    out: &Path,
) -> Report {
    let notes_text = match notes.map(std::fs::read_to_string).transpose() {
        Ok(t) => t,
        Err(e) => return refuse(format!("cannot read the notes file: {e}")),
    };
    let plan = &resolved.plan;
    let artifact = &plan.artifact_path;
    let (build_dir, _) = locate_hex(artifact);
    let mut built = false;
    if artifact_is_stale(artifact, &plan.cwd, &build_dir) {
        let outcome = match locks.run_build(plan).await {
            Ok(o) => o,
            Err(e) => return refuse(format!("failed to run build for '{}': {e:#}", project.name)),
        };
        if !outcome.ready_to_flash() {
            return refuse(format!(
                "the artifact is older than its sources and the build did not replace it: {}",
                outcome.failure_reason()
            ));
        }
        built = true;
    }
    let (build_dir, hex_path) = locate_hex(artifact);
    if artifact_is_stale(&hex_path, &plan.cwd, &build_dir) {
        return refuse(format!("{} is older than its sources even after a build: refusing to package it", hex_path.display()));
    }

    let packaged = (|| -> Result<(Written, String)> {
        let hex_text = std::fs::read_to_string(&hex_path).with_context(|| format!("reading {}", hex_path.display()))?;
        let (bootloader, app) = build_partitions(artifact, &build_dir)?;
        let segments = check_segments(&hex_segments(&hex_text)?, &bootloader, &app)?;
        let (sha, dirty) = commit_of(&project.source_path)?;
        let stem = format!("{}-{sha}", safe_name(&project.name));
        let chip = resolved.chip.as_str();
        let readme = render_readme(
            &Contents {
                name: &project.name,
                sha: &sha,
                dirty,
                chip,
                serial_baud: project.serial_baud,
                segments: &segments,
                notes: notes_text.as_deref(),
            },
            &format!("{stem}.hex"),
        );
        Ok((write_bundle(out, &stem, hex_text.as_bytes(), &readme)?, segments))
    })();
    match packaged {
        Ok((w, segments)) => Report {
            success: true,
            value: serde_json::json!({
                "success": true,
                "built": built,
                "folder": w.folder,
                "hex": w.hex,
                "readme": w.readme,
                "zip": w.zip,
                "segments": segments,
            }),
            human: format!("bundled '{}': {}", project.name, w.zip.display()),
        },
        Err(e) => refuse(format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ihex::Record;

    const DTS: &str = "/ {\n\tsoc {\n\t\tflash-controller@4004b000 {\n\t\t\tflash0: flash@8000000 {\n\t\t\t\tpartitions {\n\t\t\t\t\tboot_partition: partition@0 {\n\t\t\t\t\t\tlabel = \"mcuboot\";\n\t\t\t\t\t\treg = < 0x0 0xc000 >;\n\t\t\t\t\t};\n\t\t\t\t\tslot0_partition: partition@c000 {\n\t\t\t\t\t\tlabel = \"image-0\";\n\t\t\t\t\t\treg = < 0xc000 0x20000 >;\n\t\t\t\t\t};\n\t\t\t\t};\n\t\t\t};\n\t\t};\n\t};\n};\n";

    fn hex_with(runs: &[(u32, usize)]) -> String {
        let mut records = Vec::new();
        for &(addr, len) in runs {
            records.push(Record::ExtendedLinearAddress((addr >> 16) as u16));
            records.push(Record::Data { offset: (addr & 0xFFFF) as u16, value: vec![0xA5; len] });
        }
        records.push(Record::EndOfFile);
        ihex::create_object_file_representation(&records).unwrap()
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("embarch-bundle-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn partitions_come_from_the_dts_with_the_flash_base_added() {
        let parts = parse_partitions(DTS);
        assert_eq!(parts[0], Partition { label: "mcuboot".into(), address: 0x0800_0000, size: 0xc000 });
        assert_eq!(parts[1], Partition { label: "image-0".into(), address: 0x0800_c000, size: 0x20000 });
    }

    #[test]
    fn a_hex_missing_the_app_slot_is_refused_by_name() {
        let (b, a) = (
            Partition { label: "mcuboot".into(), address: 0x0800_0000, size: 0xc000 },
            Partition { label: "image-0".into(), address: 0x0800_c000, size: 0x20000 },
        );
        let both = hex_segments(&hex_with(&[(0x0800_0000, 64), (0x0800_c000, 64)])).unwrap();
        assert!(check_segments(&both, &b, &a).unwrap().contains("image-0"));
        let boot_only = hex_segments(&hex_with(&[(0x0800_0000, 64)])).unwrap();
        let e = check_segments(&boot_only, &b, &a).unwrap_err().to_string();
        assert!(e.contains("image-0") && e.contains("0x800C000"), "{e}");
    }

    #[test]
    fn readme_fills_the_hex_name_and_appends_notes() {
        let c = Contents {
            name: "p",
            sha: "abc1234",
            dirty: false,
            chip: "nrf54l15",
            serial_baud: Some(115200),
            segments: "x",
            notes: Some("bench checklist\n"),
        };
        let r = render_readme(&c, "p-abc1234.hex");
        assert!(r.contains("--firmware p-abc1234.hex --options chip_erase_mode=ERASE_RANGES_TOUCHED_BY_FIRMWARE"), "{r}");
        assert!(!r.contains("ERASE_ALL"), "{r}");
        assert!(r.contains("not yet confirmed on a board"), "{r}");
        assert!(r.contains("-device nRF54L15_M33"), "{r}");
        assert!(r.contains("Reset") && r.contains("Recover") && r.contains("115200 baud"), "{r}");
        assert!(r.trim_end().ends_with("bench checklist"), "{r}");
    }

    #[test]
    fn an_unrecorded_family_gets_a_line_saying_so_and_no_command() {
        let c = Contents { name: "p", sha: "s", dirty: false, chip: "xyz9000", serial_baud: None, segments: "x", notes: None };
        let r = render_readme(&c, "p-s.hex");
        assert!(r.contains("records no flashing, reset or recover command for chip `xyz9000`"), "{r}");
        assert!(!r.contains("nrfutil") && r.contains("records no console setting"), "{r}");
    }

    #[test]
    fn the_zip_holds_the_folder_with_both_files() {
        let out = tmp("zip");
        let w = write_bundle(&out, "p-abc", b":00000001FF\n", "# hi\n").unwrap();
        assert!(w.hex.is_file() && w.readme.is_file());
        let mut z = zip::ZipArchive::new(std::fs::File::open(&w.zip).unwrap()).unwrap();
        let names: Vec<String> = (0..z.len()).map(|i| z.by_index(i).unwrap().name().unwrap().to_string()).collect();
        assert_eq!(names, ["p-abc/", "p-abc/p-abc.hex", "p-abc/README.md"]);
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn an_artifact_older_than_its_sources_is_stale() {
        let root = tmp("stale");
        let build = root.join("build");
        std::fs::create_dir_all(build.join("zephyr")).unwrap();
        let artifact = build.join("zephyr").join("zephyr.hex");
        std::fs::write(&artifact, "x").unwrap();
        let old = SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options().write(true).open(&artifact).unwrap().set_modified(old).unwrap();
        assert!(!artifact_is_stale(&artifact, &root, &build), "the build directory is not a source");
        std::fs::write(root.join("main.c"), "int main;").unwrap();
        assert!(artifact_is_stale(&artifact, &root, &build));
        assert!(artifact_is_stale(&root.join("missing.hex"), &root, &build));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn merged_hex_is_preferred_and_partitions_are_found_in_the_build() {
        let root = tmp("locate");
        let build = root.join("build");
        std::fs::create_dir_all(build.join("app").join("zephyr")).unwrap();
        std::fs::create_dir_all(build.join("zephyr")).unwrap();
        std::fs::write(build.join("merged.hex"), "x").unwrap();
        std::fs::write(build.join("app").join("zephyr").join("zephyr.dts"), DTS).unwrap();
        let artifact = build.join("zephyr").join("zephyr.hex");
        let (dir, hex) = locate_hex(&artifact);
        assert_eq!((dir, hex), (build.clone(), build.join("merged.hex")));
        let (b, a) = build_partitions(&artifact, &build).unwrap();
        assert_eq!((b.label.as_str(), a.address), ("mcuboot", 0x0800_c000));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A synthetic project: a git tree with one source file, a sysbuild-shaped
    /// build directory beside it, and a no-op build command.
    fn synthetic(tag: &str, hex: &str) -> (PathBuf, ProjectConfig, Resolved) {
        let root = tmp(tag);
        let src = root.join("src");
        let build = root.join("out");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(build.join("zephyr")).unwrap();
        std::fs::create_dir_all(build.join("app").join("zephyr")).unwrap();
        std::fs::write(src.join("main.c"), "int main;").unwrap();
        std::fs::write(build.join("merged.hex"), hex).unwrap();
        std::fs::write(build.join("zephyr").join("zephyr.hex"), hex).unwrap();
        std::fs::write(build.join("app").join("zephyr").join("zephyr.dts"), DTS).unwrap();
        for args in [["init", "-q"].as_slice(), &["add", "."], &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "i"]] {
            let ok = std::process::Command::new("git").arg("-C").arg(&src).args(args).status().unwrap().success();
            assert!(ok, "git {args:?}");
        }
        let project: ProjectConfig = toml::from_str(&format!(
            "name = \"synth\"\nsource_path = \"{}\"\nflash_format = \"hex\"\nchip = \"nrf54l15\"\nserial_baud = 115200\n\
             build_command = [\"true\"]\nartifact_path = \"{}\"\n",
            src.display(),
            build.join("zephyr").join("zephyr.hex").display()
        ))
        .unwrap();
        let resolved = Resolved {
            plan: embarch_firmware_build::build::BuildPlan {
                lock_key: tag.to_string(),
                cwd: src.clone(),
                command: vec!["true".into()],
                artifact_path: build.join("zephyr").join("zephyr.hex"),
                timeout_secs: 30,
                env: Default::default(),
                manifest: None,
            },
            chip: "nrf54l15".into(),
            flash_format: "hex".into(),
            base_address: None,
            probe_serial: None,
            descriptor: serde_json::Value::Null,
            pristine: None,
        };
        (root, project, resolved)
    }

    #[tokio::test]
    async fn a_stale_artifact_that_the_build_does_not_replace_is_refused_and_a_fresh_one_is_bundled() {
        let hex = hex_with(&[(0x0800_0000, 64), (0x0800_c000, 64)]);
        let (root, project, resolved) = synthetic("e2e", &hex);
        let notes = root.join("notes.md");
        std::fs::write(&notes, "check the LED\n").unwrap();
        let out = root.join("bundles");
        let locks = BuildLocks::with_lock_dir(root.join("locks"));

        // Artifacts an hour old, a source file written just now.
        let old = SystemTime::now() - std::time::Duration::from_secs(3600);
        for f in [root.join("out/merged.hex"), root.join("out/zephyr/zephyr.hex")] {
            std::fs::File::options().write(true).open(f).unwrap().set_modified(old).unwrap();
        }
        let r = bundle(&locks, &project, &resolved, Some(&notes), &out).await;
        assert!(!r.success, "{}", r.human);
        assert!(!out.exists(), "a refusal writes nothing");

        // The build touched the artifact: now it is fresh.
        for f in [root.join("out/merged.hex"), root.join("out/zephyr/zephyr.hex")] {
            std::fs::File::options().write(true).open(f).unwrap().set_modified(SystemTime::now()).unwrap();
        }
        let r = bundle(&locks, &project, &resolved, Some(&notes), &out).await;
        assert!(r.success, "{}", r.human);
        let folder = std::fs::read_dir(&out).unwrap().flatten().find(|e| e.path().is_dir()).unwrap().path();
        let stem = folder.file_name().unwrap().to_string_lossy().to_string();
        assert!(stem.starts_with("synth-"), "{stem}");
        assert!(out.join(format!("{stem}.zip")).is_file());
        let readme = std::fs::read_to_string(folder.join("README.md")).unwrap();
        assert!(readme.contains(&format!("--firmware {stem}.hex")) && readme.contains("check the LED"), "{readme}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn an_incomplete_hex_is_refused_before_anything_is_written() {
        let (root, project, resolved) = synthetic("incomplete", &hex_with(&[(0x0800_0000, 64)]));
        let out = root.join("bundles");
        let locks = BuildLocks::with_lock_dir(root.join("locks"));
        let r = bundle(&locks, &project, &resolved, None, &out).await;
        assert!(!r.success && r.human.contains("image-0"), "{}", r.human);
        assert!(!out.exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
