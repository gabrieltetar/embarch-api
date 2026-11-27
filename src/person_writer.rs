//! The confined writer for the person's `embarch/` folder (decision 132).
//!
//! It writes only under `<repo>/embarch`, only a file of a known kind, under a
//! name from a fixed set, and follows no link at any step. It runs nothing: this
//! module starts no process, and a test reads its own source to say so.
//! `hil_repo` writes the test layout through it; `live_log.rs` still joins paths.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Result};

/// The longest name, in bytes, before the kind's extension is appended.
const NAME_MAX: usize = 100;

/// What a file is; each kind has one folder and one extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Study,
    HostStudy,
    /// `tests/suites/<suite>/<id>/test.json`; the name is `<suite>/<id>`.
    Test,
    /// A test's study beside it, `tests/suites/<suite>/<id>/<file>`; the name is `<suite>/<id>/<file>`.
    TestStudy,
    /// `tests/suites/<suite>/suite.json`; the name is the suite's.
    Suite,
    /// `tests/plans/<plan>.json`.
    Plan,
    TestResults,
    SavedBench,
    AtlasMeasured,
    LiveLog,
    /// `studies/<study>/report.typ`; the name is the study's.
    ReportTyp,
    /// `studies/<study>/runs/<run>.md`; the name is `<study>/<run>`.
    Prose,
}

impl Kind {
    /// The kind a caller names, or a refusal for anything outside the set.
    pub fn parse(s: &str) -> Result<Kind> {
        Ok(match s {
            "study" => Kind::Study,
            "host_study" => Kind::HostStudy,
            "test" => Kind::Test,
            "test_study" => Kind::TestStudy,
            "suite" => Kind::Suite,
            "plan" => Kind::Plan,
            "test_results" => Kind::TestResults,
            "saved_bench" => Kind::SavedBench,
            "atlas_measured" => Kind::AtlasMeasured,
            "live_log" => Kind::LiveLog,
            "report_typ" => Kind::ReportTyp,
            "prose" => Kind::Prose,
            other => bail!("refused: `{other}` is not a kind this writer writes"),
        })
    }

    /// The folder under `embarch/` and the extension appended to the file name.
    fn folder_and_ext(self) -> (&'static str, &'static str) {
        match self {
            Kind::Study => ("studies", ".study.json"),
            Kind::HostStudy => ("studies", ".host-study.json"),
            Kind::Test | Kind::TestStudy | Kind::Suite => ("tests", ""),
            Kind::Plan => ("tests", ".json"),
            Kind::TestResults => ("tests", ".results.json"),
            Kind::SavedBench => ("topologies", ".toml"),
            Kind::AtlasMeasured => ("atlas", ".json"),
            Kind::LiveLog => ("live", ".jsonl"),
            Kind::ReportTyp => ("studies", ".typ"),
            Kind::Prose => ("studies", ".md"),
        }
    }
}

/// `[a-z0-9][a-z0-9._-]{0,99}`, and no `..` anywhere.
fn valid_name(s: &str) -> bool {
    let b = s.as_bytes();
    if b.is_empty() || b.len() > NAME_MAX || s.contains("..") {
        return false;
    }
    let first = b[0];
    (first.is_ascii_lowercase() || first.is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-'))
}

/// A resolved target: the folders under `embarch/` and the file name.
struct Target {
    folders: Vec<String>,
    file: String,
}

/// A name of exactly `N` slash-separated parts, or a refusal saying the shape.
fn parts<'a, const N: usize>(name: &'a str, what: &str, shape: &str) -> Result<[&'a str; N]> {
    let v: Vec<&str> = name.split('/').collect();
    match <[&str; N]>::try_from(v) {
        Ok(a) => Ok(a),
        Err(_) => bail!("refused: a {what} name is {shape}"),
    }
}

fn resolve(kind: Kind, name: &str) -> Result<Target> {
    let (folder, ext) = kind.folder_and_ext();
    let check = |n: &str| -> Result<()> {
        if !valid_name(n) {
            bail!("refused: `{n}` is not a name this writer accepts ([a-z0-9][a-z0-9._-]{{0,99}}, no `..`)");
        }
        Ok(())
    };
    match kind {
        Kind::ReportTyp => {
            check(name)?;
            Ok(Target { folders: vec![folder.into(), name.into()], file: format!("report{ext}") })
        }
        Kind::Test => {
            let [suite, id] = parts(name, "test", "`<suite>/<id>`")?;
            check(suite)?;
            check(id)?;
            Ok(Target { folders: vec![folder.into(), "suites".into(), suite.into(), id.into()], file: "test.json".into() })
        }
        Kind::TestStudy => {
            let [suite, id, file] = parts(name, "test study", "`<suite>/<id>/<file>`")?;
            check(suite)?;
            check(id)?;
            check(file)?;
            Ok(Target { folders: vec![folder.into(), "suites".into(), suite.into(), id.into()], file: file.into() })
        }
        Kind::Suite => {
            check(name)?;
            Ok(Target { folders: vec![folder.into(), "suites".into(), name.into()], file: "suite.json".into() })
        }
        Kind::Plan => {
            check(name)?;
            Ok(Target { folders: vec![folder.into(), "plans".into()], file: format!("{name}{ext}") })
        }
        Kind::Prose => {
            let Some((study, run)) = name.split_once('/') else {
                bail!("refused: a prose name is `<study>/<run>`");
            };
            check(study)?;
            check(run)?;
            Ok(Target { folders: vec![folder.into(), study.into(), "runs".into()], file: format!("{run}{ext}") })
        }
        _ => {
            check(name)?;
            Ok(Target { folders: vec![folder.into()], file: format!("{name}{ext}") })
        }
    }
}

/// A writer bound to one repository's `embarch/` folder.
pub struct ConfinedWriter {
    root: sys::Root,
}

impl ConfinedWriter {
    /// Opens `<repo_root>/embarch`, refusing when it is a link or not a folder.
    pub fn open(repo_root: &Path) -> Result<ConfinedWriter> {
        Ok(ConfinedWriter { root: sys::open_root(repo_root)? })
    }

    /// Writes `bytes` as one file of `kind`, atomically, and returns the path
    /// below `embarch/` it landed at.
    pub fn write(&self, kind: Kind, name: &str, bytes: &[u8]) -> Result<String> {
        Ok(self.write_with(kind, name, bytes, true)?.0)
    }

    /// Like `write`; `overwrite` false refuses an existing file. Also returns
    /// whether the write replaced one.
    pub fn write_with(&self, kind: Kind, name: &str, bytes: &[u8], overwrite: bool) -> Result<(String, bool)> {
        let t = resolve(kind, name)?;
        let replaced = sys::write_file(&self.root, &t.folders, &t.file, bytes, Finish::Rename, overwrite)?;
        Ok((format!("{}/{}", t.folders.join("/"), t.file), replaced))
    }

    #[cfg(test)]
    fn write_temp_only(&self, kind: Kind, name: &str, bytes: &[u8]) -> Result<()> {
        let t = resolve(kind, name)?;
        sys::write_file(&self.root, &t.folders, &t.file, bytes, Finish::LeaveTemp, true).map(|_| ())
    }
}

/// How far a write goes; `LeaveTemp` simulates a crash before the rename.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Finish {
    Rename,
    #[cfg_attr(not(test), allow(dead_code))]
    LeaveTemp,
}

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// A temp name that no valid name can equal (it starts with a dot).
fn temp_name() -> String {
    format!(".tmp-{}-{}", std::process::id(), TEMP_SEQ.fetch_add(1, Ordering::Relaxed))
}

#[cfg(unix)]
mod sys {
    use super::{temp_name, Finish};
    use anyhow::{bail, Context, Result};
    use rustix::fs::{fsync, mkdirat, openat, renameat, statat, unlinkat, AtFlags, FileType, Mode, OFlags, CWD};
    use rustix::io::Errno;
    use std::io::Write;
    use std::os::fd::OwnedFd;
    use std::path::Path;

    pub struct Root(OwnedFd);

    const DIR_FLAGS: OFlags = OFlags::RDONLY.union(OFlags::DIRECTORY).union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);

    pub fn open_root(repo: &Path) -> Result<Root> {
        let repo_fd = openat(CWD, repo, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())
            .with_context(|| format!("opening {}", repo.display()))?;
        match openat(&repo_fd, "embarch", DIR_FLAGS, Mode::empty()) {
            Ok(fd) => Ok(Root(fd)),
            Err(Errno::LOOP) | Err(Errno::NOTDIR) => bail!("refused: {}/embarch is a link or not a folder", repo.display()),
            Err(e) => bail!("refused: {}/embarch cannot be opened: {e}", repo.display()),
        }
    }

    fn open_dir(parent: &OwnedFd, name: &str) -> Result<OwnedFd> {
        let mut made = false;
        loop {
            match openat(parent, name, DIR_FLAGS, Mode::empty()) {
                Ok(fd) => return Ok(fd),
                Err(Errno::NOENT) if !made => {
                    match mkdirat(parent, name, Mode::from_raw_mode(0o755)) {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(e) => bail!("creating folder `{name}`: {e}"),
                    }
                    made = true;
                }
                Err(Errno::LOOP) | Err(Errno::NOTDIR) => bail!("refused: `{name}` is a link or not a folder"),
                Err(e) => bail!("opening folder `{name}`: {e}"),
            }
        }
    }

    pub fn write_file(root: &Root, folders: &[String], file: &str, bytes: &[u8], finish: Finish, overwrite: bool) -> Result<bool> {
        let mut cur = open_dir(&root.0, &folders[0])?;
        for f in &folders[1..] {
            cur = open_dir(&cur, f)?;
        }
        let existed = match statat(&cur, file, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => {
                if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
                    bail!("refused: `{file}` exists and is not a plain file");
                }
                if st.st_nlink > 1 {
                    bail!("refused: `{file}` has {} hard links", st.st_nlink);
                }
                true
            }
            Err(Errno::NOENT) => false,
            Err(e) => bail!("reading `{file}`: {e}"),
        };
        if existed && !overwrite {
            bail!("refused: `{file}` already exists");
        }
        let tmp = temp_name();
        let fd = openat(&cur, tmp.as_str(), OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::from_raw_mode(0o644))
            .with_context(|| format!("creating temp file `{tmp}`"))?;
        let mut f = std::fs::File::from(fd);
        let wrote = f.write_all(bytes).map_err(anyhow::Error::from).and_then(|()| Ok(fsync(&f)?));
        drop(f);
        if let Err(e) = wrote {
            let _ = unlinkat(&cur, tmp.as_str(), rustix::fs::AtFlags::empty());
            return Err(e.context(format!("writing `{file}`")));
        }
        if finish == Finish::LeaveTemp {
            return Ok(existed);
        }
        if let Err(e) = renameat(&cur, tmp.as_str(), &cur, file) {
            let _ = unlinkat(&cur, tmp.as_str(), rustix::fs::AtFlags::empty());
            bail!("renaming onto `{file}`: {e}");
        }
        Ok(existed)
    }
}

#[cfg(windows)]
mod sys {
    use super::{temp_name, Finish};
    use anyhow::{bail, Context, Result};
    use std::io::Write;
    use std::os::windows::fs::MetadataExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};
    use windows_sys::Win32::Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT};

    pub struct Root(PathBuf);

    /// A folder step: refuse a link or reparse point, and anything not a folder.
    fn check_dir(p: &Path) -> Result<()> {
        let md = std::fs::symlink_metadata(p).with_context(|| format!("reading {}", p.display()))?;
        if md.file_type().is_symlink() || md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            bail!("refused: {} is a link", p.display());
        }
        if !md.is_dir() {
            bail!("refused: {} is not a folder", p.display());
        }
        Ok(())
    }

    pub fn open_root(repo: &Path) -> Result<Root> {
        let p = repo.join("embarch");
        check_dir(&p)?;
        Ok(Root(p))
    }

    pub fn write_file(root: &Root, folders: &[String], file: &str, bytes: &[u8], finish: Finish, overwrite: bool) -> Result<bool> {
        let mut cur = root.0.clone();
        check_dir(&cur)?;
        for f in folders {
            cur.push(f);
            match std::fs::symlink_metadata(&cur) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir(&cur).with_context(|| format!("creating {}", cur.display()))?,
                Err(e) => bail!("reading {}: {e}", cur.display()),
            }
            check_dir(&cur)?;
        }
        let target = cur.join(file);
        let existed = match std::fs::symlink_metadata(&target) {
            Ok(md) => {
                if md.file_type().is_symlink() || md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !md.is_file() {
                    bail!("refused: `{file}` exists and is not a plain file");
                }
                let h = std::fs::File::open(&target)?;
                let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
                // SAFETY: the handle is live for the call and `info` is a valid out-pointer.
                if unsafe { GetFileInformationByHandle(h.as_raw_handle() as _, &mut info) } == 0 {
                    bail!("reading the link count of `{file}`");
                }
                if info.nNumberOfLinks > 1 {
                    bail!("refused: `{file}` has {} hard links", info.nNumberOfLinks);
                }
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => bail!("reading `{file}`: {e}"),
        };
        if existed && !overwrite {
            bail!("refused: `{file}` already exists");
        }
        let tmp = cur.join(temp_name());
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        let wrote = f.write_all(bytes).and_then(|()| f.sync_all());
        drop(f);
        if let Err(e) = wrote {
            let _ = std::fs::remove_file(&tmp);
            return Err(anyhow::Error::from(e).context(format!("writing `{file}`")));
        }
        if finish == Finish::LeaveTemp {
            return Ok(existed);
        }
        if let Err(e) = std::fs::rename(&tmp, &target) {
            let _ = std::fs::remove_file(&tmp);
            bail!("renaming onto `{file}`: {e}");
        }
        Ok(existed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new() -> Tmp {
            static N: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!("pw-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
            std::fs::create_dir_all(p.join("embarch")).unwrap();
            Tmp(p)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn refused<T>(r: Result<T>) -> String {
        match r {
            Ok(_) => panic!("expected a refusal"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn a_clean_write_of_each_kind_lands_at_its_path() {
        let t = Tmp::new();
        let w = ConfinedWriter::open(&t.0).unwrap();
        let cases = [
            (Kind::Study, "s1", "studies/s1.study.json"),
            (Kind::HostStudy, "s1", "studies/s1.host-study.json"),
            (Kind::Test, "su/t-1", "tests/suites/su/t-1/test.json"),
            (Kind::TestStudy, "su/t-1/study.json", "tests/suites/su/t-1/study.json"),
            (Kind::Suite, "su", "tests/suites/su/suite.json"),
            (Kind::Plan, "p", "tests/plans/p.json"),
            (Kind::TestResults, "r", "tests/r.results.json"),
            (Kind::SavedBench, "b", "topologies/b.toml"),
            (Kind::AtlasMeasured, "a", "atlas/a.json"),
            (Kind::LiveLog, "2026-11-25-x", "live/2026-11-25-x.jsonl"),
            (Kind::ReportTyp, "s1", "studies/s1/report.typ"),
            (Kind::Prose, "s1/run-7", "studies/s1/runs/run-7.md"),
        ];
        for (kind, name, rel) in cases {
            let got = w.write(kind, name, b"one").unwrap();
            assert_eq!(got, rel);
            assert_eq!(std::fs::read(t.0.join("embarch").join(rel)).unwrap(), b"one");
            w.write(kind, name, b"two").unwrap();
            assert_eq!(std::fs::read(t.0.join("embarch").join(rel)).unwrap(), b"two");
        }
    }

    #[test]
    fn an_unknown_kind_is_refused() {
        assert!(refused(Kind::parse("shell_script")).contains("not a kind"));
        assert_eq!(Kind::parse("saved_bench").unwrap(), Kind::SavedBench);
    }

    #[test]
    fn a_dotdot_name_and_a_name_outside_the_set_are_refused() {
        let t = Tmp::new();
        let w = ConfinedWriter::open(&t.0).unwrap();
        for bad in ["..", "a..b", "../x", "A", "-a", ".a", "", "a b", "a/b", "é", &"a".repeat(101)] {
            refused(w.write(Kind::Study, bad, b"x"));
        }
        refused(w.write(Kind::Prose, "only-one-part", b"x"));
        refused(w.write(Kind::Prose, "s/../x", b"x"));
        refused(w.write(Kind::Prose, "s/r/extra", b"x"));
        assert!(w.write(Kind::Study, &"a".repeat(100), b"x").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn embarch_as_a_symlink_is_refused() {
        let t = Tmp::new();
        std::fs::remove_dir(t.0.join("embarch")).unwrap();
        let elsewhere = Tmp::new();
        std::os::unix::fs::symlink(elsewhere.0.join("embarch"), t.0.join("embarch")).unwrap();
        assert!(refused(ConfinedWriter::open(&t.0).map(|_| ())).contains("link or not a folder"));
    }

    #[test]
    fn embarch_that_is_a_file_is_refused() {
        let t = Tmp::new();
        std::fs::remove_dir(t.0.join("embarch")).unwrap();
        std::fs::write(t.0.join("embarch"), b"x").unwrap();
        refused(ConfinedWriter::open(&t.0).map(|_| ()));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_subfolder_is_refused() {
        let t = Tmp::new();
        let elsewhere = Tmp::new();
        std::os::unix::fs::symlink(&elsewhere.0, t.0.join("embarch").join("studies")).unwrap();
        let w = ConfinedWriter::open(&t.0).unwrap();
        refused(w.write(Kind::Study, "s", b"x"));
        assert!(!elsewhere.0.join("s.study.json").exists());
        // The same for a study's own folder, one step deeper.
        std::fs::remove_file(t.0.join("embarch").join("studies")).unwrap();
        std::fs::create_dir(t.0.join("embarch").join("studies")).unwrap();
        std::os::unix::fs::symlink(&elsewhere.0, t.0.join("embarch").join("studies").join("s")).unwrap();
        refused(w.write(Kind::ReportTyp, "s", b"x"));
        assert!(!elsewhere.0.join("report.typ").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_target_file_is_refused() {
        let t = Tmp::new();
        let w = ConfinedWriter::open(&t.0).unwrap();
        w.write(Kind::Study, "s", b"x").unwrap();
        let victim = t.0.join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        let target = t.0.join("embarch/studies/s.study.json");
        std::fs::remove_file(&target).unwrap();
        std::os::unix::fs::symlink(&victim, &target).unwrap();
        refused(w.write(Kind::Study, "s", b"x"));
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_at_a_suite_folder_or_a_plan_path_is_refused() {
        let t = Tmp::new();
        let elsewhere = Tmp::new();
        let w = ConfinedWriter::open(&t.0).unwrap();
        let suites = t.0.join("embarch/tests/suites");
        std::fs::create_dir_all(&suites).unwrap();
        std::os::unix::fs::symlink(&elsewhere.0, suites.join("su")).unwrap();
        refused(w.write(Kind::Suite, "su", b"x"));
        refused(w.write(Kind::Test, "su/t", b"x"));
        refused(w.write(Kind::TestStudy, "su/t/study.json", b"x"));
        assert!(!elsewhere.0.join("suite.json").exists() && !elsewhere.0.join("t").exists());
        let victim = t.0.join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        let plans = t.0.join("embarch/tests/plans");
        std::fs::create_dir_all(&plans).unwrap();
        std::os::unix::fs::symlink(&victim, plans.join("p.json")).unwrap();
        refused(w.write(Kind::Plan, "p", b"x"));
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
    }

    #[test]
    fn a_hard_linked_plan_is_refused_and_no_overwrite_keeps_an_existing_file() {
        let t = Tmp::new();
        let w = ConfinedWriter::open(&t.0).unwrap();
        w.write(Kind::Plan, "p", b"old").unwrap();
        let other = t.0.join("other");
        std::fs::hard_link(t.0.join("embarch/tests/plans/p.json"), &other).unwrap();
        assert!(refused(w.write(Kind::Plan, "p", b"new")).contains("hard links"));
        assert_eq!(std::fs::read(&other).unwrap(), b"old");
        assert!(!w.write_with(Kind::SavedBench, "b", b"1", false).unwrap().1);
        assert!(refused(w.write_with(Kind::SavedBench, "b", b"2", false)).contains("already exists"));
        assert!(w.write_with(Kind::SavedBench, "b", b"3", true).unwrap().1);
    }

    #[test]
    fn a_target_with_two_hard_links_is_refused() {
        let t = Tmp::new();
        let w = ConfinedWriter::open(&t.0).unwrap();
        w.write(Kind::Study, "s", b"old").unwrap();
        let other = t.0.join("other");
        std::fs::hard_link(t.0.join("embarch/studies/s.study.json"), &other).unwrap();
        assert!(refused(w.write(Kind::Study, "s", b"new")).contains("hard links"));
        assert_eq!(std::fs::read(&other).unwrap(), b"old");
    }

    #[test]
    fn a_crash_between_the_temp_write_and_the_rename_leaves_the_old_file_whole() {
        let t = Tmp::new();
        let w = ConfinedWriter::open(&t.0).unwrap();
        w.write(Kind::Study, "s", b"old").unwrap();
        w.write_temp_only(Kind::Study, "s", b"new").unwrap();
        let dir = t.0.join("embarch/studies");
        assert_eq!(std::fs::read(dir.join("s.study.json")).unwrap(), b"old");
        let temps: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with(".tmp-")).collect();
        assert_eq!(temps.len(), 1);
        // A later clean write still lands, and the stale temp never matches a name.
        w.write(Kind::Study, "s", b"newer").unwrap();
        assert_eq!(std::fs::read(dir.join("s.study.json")).unwrap(), b"newer");
        assert!(!valid_name(&temps[0]));
    }

    #[test]
    fn the_module_holds_no_command_and_no_spawn() {
        let src = include_str!("person_writer.rs");
        let production = src.split("#[cfg(test)]\nmod tests").next().unwrap();
        // Built from pieces so this test does not match itself.
        for word in [["Comm", "and"].concat(), ["spa", "wn"].concat(), ["process::", "Command"].concat(), ["shell", "_exec"].concat()] {
            assert!(!production.contains(&word), "the module mentions `{word}`");
        }
    }
}
