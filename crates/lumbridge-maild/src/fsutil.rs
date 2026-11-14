//! Small file helpers: private directories and files, safe names for files mail tells us about.

use lumbridge_mail_core::{Error, Result};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub fn chmod(path: &Path, mode: u32) {
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

pub fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Creates `dir` (and parents) and makes it 0700. Refuses a directory owned by someone else (a shared
/// /tmp fallback where another user got there first), or a symlink.
pub fn private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| Error::Other(format!("{}: {e}", dir.display())))?;
    let meta = std::fs::symlink_metadata(dir).map_err(|e| Error::Other(format!("{}: {e}", dir.display())))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(Error::Refused(format!("{} is not a plain directory", dir.display())));
    }
    if meta.uid() != euid() {
        return Err(Error::Refused(format!("{} belongs to another user", dir.display())));
    }
    chmod(dir, 0o700);
    Ok(())
}

/// Writes `bytes` to `path` with mode 0600 (created that way, never world-readable for a moment).
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| Error::Other(format!("{}: {e}", tmp.display())))?;
    f.write_all(bytes).and_then(|_| f.sync_all()).map_err(|e| Error::Other(format!("{}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, path).map_err(|e| Error::Other(format!("{}: {e}", path.display())))
}

/// 32 random bytes as hex.
pub fn random_token() -> Result<String> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| Error::Other(format!("no randomness: {e}")))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// A file name from mail made safe to create (`core::files::safe_file_name`: the app applies the same
/// rule to its own cache).
pub fn safe_file_name(name: &str) -> String {
    lumbridge_mail_core::files::safe_file_name(name)
}

/// A short stable directory name for an id that comes from a server (never used as a path as is).
pub fn hashed(parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0]);
    }
    h.finalize().iter().take(12).map(|x| format!("{x:02x}")).collect()
}

/// `dir/<name>`, or `dir/<stem> (2).<ext>` ... if taken by different content.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_owned(), format!(".{e}")),
        _ => (name.to_owned(), String::new()),
    };
    for n in 2..1000 {
        let p = dir.join(format!("{stem} ({n}){ext}"));
        if !p.exists() {
            return p;
        }
    }
    dir.join(format!("{stem}-{}{ext}", hashed(&[name])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_from_mail_cannot_escape() {
        assert_eq!(safe_file_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_file_name("..\\..\\boot.ini"), "boot.ini");
        assert_eq!(safe_file_name(".bashrc"), "bashrc");
        assert_eq!(safe_file_name("a\u{0}b\nc.pdf"), "a_b_c.pdf");
        assert_eq!(safe_file_name(""), "attachment");
        assert_eq!(safe_file_name(".."), "attachment");
        assert_eq!(safe_file_name("Q3 report.pdf"), "Q3 report.pdf");
        assert_eq!(safe_file_name(&"x".repeat(500)).len(), 120);
    }

    #[test]
    fn private_files_and_dirs() {
        let d = tempfile::tempdir().unwrap();
        let sub = d.path().join("a/b");
        private_dir(&sub).unwrap();
        assert_eq!(std::fs::metadata(&sub).unwrap().permissions().mode() & 0o777, 0o700);
        let f = sub.join("t");
        write_private(&f, b"x").unwrap();
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(random_token().unwrap().len(), 64);
        std::fs::write(sub.join("r.pdf"), b"1").unwrap();
        assert_eq!(unique_path(&sub, "r.pdf"), sub.join("r (2).pdf"));
    }
}
