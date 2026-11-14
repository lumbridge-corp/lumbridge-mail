//! Attachments on this machine: copying one in from a daemon on another machine (in pieces, into our
//! private cache), opening it with the system's program, saving a copy to Downloads.
//!
//! The rules are core's (`lumbridge_mail_core::files`): names from mail go through `safe_file_name`
//! (no directories, no leading dots), nothing from mail ever gets an executable bit (files are created
//! 0600 in 0700 directories), and an attachment that `dangerous_to_open` flags is never opened by the
//! app: the person can save it and decide.

use lumbridge_mail_core::files::safe_file_name;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The largest attachment copied from a daemon on another machine.
pub const MAX_REMOTE_BYTES: u64 = 256 << 20;

/// Opens files and folders with the system's programs. Tests use [`Recorder`].
pub trait Opener: Send + Sync {
    /// Open `path` with its default program.
    fn open(&self, path: &Path) -> Result<(), String>;
    /// Show `path` in the file manager.
    fn reveal(&self, path: &Path) -> Result<(), String>;
}

/// `xdg-open` on Linux, `open` on macOS. The path is one argument, never through a shell.
pub struct System;

impl System {
    fn run(mut c: Command) -> Result<(), String> {
        c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        c.spawn().map(|_| ()).map_err(|e| format!("could not start the opener: {e}"))
    }
}

impl Opener for System {
    fn open(&self, path: &Path) -> Result<(), String> {
        let mut c = Command::new(if cfg!(target_os = "macos") { "open" } else { "xdg-open" });
        c.arg(path);
        Self::run(c)
    }
    fn reveal(&self, path: &Path) -> Result<(), String> {
        if cfg!(target_os = "macos") {
            let mut c = Command::new("open");
            c.arg("-R").arg(path);
            return Self::run(c);
        }
        let mut c = Command::new("xdg-open");
        c.arg(path.parent().unwrap_or(path));
        Self::run(c)
    }
}

/// Remembers what it was asked to open (tests, screenshots).
#[derive(Default)]
pub struct Recorder {
    pub opened: parking_lot::Mutex<Vec<PathBuf>>,
    pub revealed: parking_lot::Mutex<Vec<PathBuf>>,
}

impl Opener for Recorder {
    fn open(&self, path: &Path) -> Result<(), String> {
        self.opened.lock().push(path.to_owned());
        Ok(())
    }
    fn reveal(&self, path: &Path) -> Result<(), String> {
        self.revealed.lock().push(path.to_owned());
        Ok(())
    }
}

/// Where "Save" puts files: `$XDG_DOWNLOAD_DIR`, else `~/Downloads`, else the home directory.
pub fn downloads_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("XDG_DOWNLOAD_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d));
    }
    let home = PathBuf::from(std::env::var_os("HOME").filter(|h| !h.is_empty())?);
    let dl = home.join("Downloads");
    Some(if dl.is_dir() { dl } else { home })
}

fn mode_private(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Creates `dir` (and parents) as 0700 and refuses a symlink or a directory of another user.
pub fn private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let meta = std::fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    if meta.file_type().is_symlink() || !meta.is_dir() || meta.uid() != me {
        return Err(format!("{} is not a private directory of this user", dir.display()));
    }
    mode_private(dir, 0o700).map_err(|e| format!("{}: {e}", dir.display()))
}

/// `dir/<name>`, or `dir/<stem> (2).<ext>` ... when taken.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_owned(), format!(".{e}")),
        _ => (name.to_owned(), String::new()),
    };
    (2..10_000).map(|n| dir.join(format!("{stem} ({n}){ext}"))).find(|p| !p.exists()).unwrap_or(first)
}

/// FNV-1a, 64 bit: a short directory name for ids from a server (never used as a path as is).
pub fn hashed(parts: &[&str]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for b in p.bytes().chain(std::iter::once(0)) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{h:016x}")
}

/// A file being written from pieces: created 0600 under a temporary name next to where it ends up,
/// renamed into place only when complete, removed when dropped unfinished.
pub struct Incoming {
    tmp: PathBuf,
    file: Option<std::fs::File>,
    pub written: u64,
}

impl Incoming {
    pub fn create(dir: &Path) -> Result<Self, String> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut n = 0u32;
        loop {
            let tmp = dir.join(format!(".incoming-{}-{n}", std::process::id()));
            match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp) {
                Ok(file) => return Ok(Self { tmp, file: Some(file), written: 0 }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 1000 => n += 1,
                Err(e) => return Err(format!("{}: {e}", tmp.display())),
            }
        }
    }

    pub fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        let f = self.file.as_mut().ok_or("already finished")?;
        f.write_all(bytes).map_err(|e| format!("{}: {e}", self.tmp.display()))?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    /// Flushes and renames the file to `dest` (0600, no executable bit).
    pub fn finish(mut self, dest: &Path) -> Result<(), String> {
        let f = self.file.take().ok_or("already finished")?;
        f.sync_all().map_err(|e| format!("{}: {e}", self.tmp.display()))?;
        drop(f);
        std::fs::rename(&self.tmp, dest).map_err(|e| format!("{}: {e}", dest.display()))?;
        mode_private(dest, 0o600).map_err(|e| format!("{}: {e}", dest.display()))
    }
}

impl Drop for Incoming {
    fn drop(&mut self) {
        if self.file.take().is_some() {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

/// Copies an attachment (already on this machine) to `dir` under its safe name, never overwriting:
/// `report (2).pdf` when `report.pdf` is there. The copy is 0600 (no executable bit) whatever the
/// source's mode. Returns where it went.
pub fn save_copy(src: &Path, dir: &Path, filename: &str) -> Result<PathBuf, String> {
    use std::io::Read;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut input = std::fs::File::open(src).map_err(|e| format!("{}: {e}", src.display()))?;
    let mut out = Incoming::create(dir)?;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = input.read(&mut buf).map_err(|e| format!("{}: {e}", src.display()))?;
        if n == 0 {
            break;
        }
        out.write(&buf[..n])?;
    }
    let dest = unique_path(dir, &safe_file_name(filename));
    out.finish(&dest)?;
    Ok(dest)
}

/// "12.4 MB", for messages about sizes.
pub fn megabytes(bytes: u64) -> String {
    let mb = bytes as f64 / (1u64 << 20) as f64;
    if mb >= 10.0 { format!("{mb:.0} MB") } else { format!("{mb:.1} MB") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn incoming_files_are_private_and_vanish_when_unfinished() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("c");
        private_dir(&dir).unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        let mut a = Incoming::create(&dir).unwrap();
        a.write(b"half").unwrap();
        drop(a);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "an unfinished copy leaves nothing");
        let mut b = Incoming::create(&dir).unwrap();
        b.write(b"whole").unwrap();
        let dest = dir.join("x.pdf");
        b.finish(&dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"whole");
        assert_eq!(std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn saved_copies_keep_a_safe_name_and_never_overwrite() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("src");
        std::fs::write(&src, b"#!/bin/sh\necho made-up\n").unwrap();
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o755)).unwrap();
        let dl = d.path().join("Downloads");
        let a = save_copy(&src, &dl, "../../run.sh").unwrap();
        assert_eq!(a, dl.join("run.sh"));
        assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o600, "never executable");
        let b = save_copy(&src, &dl, "run.sh").unwrap();
        assert_eq!(b, dl.join("run (2).sh"));
        assert_eq!(std::fs::read(&b).unwrap(), b"#!/bin/sh\necho made-up\n");
    }

    #[test]
    fn small_helpers() {
        assert_eq!(hashed(&["a", "b"]), hashed(&["a", "b"]));
        assert_ne!(hashed(&["a", "b"]), hashed(&["ab", ""]));
        assert_eq!(megabytes(300 << 20), "300 MB");
        assert_eq!(megabytes(1 << 20), "1.0 MB");
    }
}
