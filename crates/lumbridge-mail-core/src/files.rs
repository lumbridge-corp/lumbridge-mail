//! Rules for files that come out of mail (attachments): a name that is safe to create on disk, and
//! whether opening one with the system's default program could run code. Pure functions, shared by the
//! daemon (its cache) and every client (the app's own cache when the daemon is on another machine), so
//! both sides apply the same rules.

/// A file name from mail made safe to create: no directories, no leading dots, no control or path
/// characters, at most 120 characters, never empty.
pub fn safe_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let clean: String = base
        .chars()
        .map(|c| if c.is_control() || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0') { '_' } else { c })
        .collect();
    let clean = clean.trim().trim_start_matches('.').trim();
    let clean: String = clean.chars().take(120).collect();
    if clean.is_empty() || clean.chars().all(|c| c == '.' || c == '_') { "attachment".into() } else { clean }
}

/// Extensions that run code (or install it, or point at something that does) when opened with the
/// system's default program, on Linux, macOS or Windows. Office files with macros are included.
const RUNS_CODE: &[&str] = &[
    // Windows
    "exe", "com", "bat", "cmd", "scr", "pif", "msi", "msp", "msc", "cpl", "dll", "sys", "vb", "vbs", "vbe", "js", "jse", "ws",
    "wsf", "wsc", "wsh", "hta", "ps1", "ps1xml", "ps2", "psc1", "psm1", "lnk", "inf", "reg", "scf", "url", "application",
    "gadget", "appx", "appxbundle", "msix", "msixbundle", "appref-ms", "chm", "settingcontent-ms", "library-ms",
    // macOS
    "app", "command", "tool", "pkg", "mpkg", "dmg", "terminal", "workflow", "scpt", "scptd", "osascript", "action",
    "prefpane", "kext", "webloc", "inetloc", "fileloc",
    // Linux / anything with an interpreter
    "sh", "bash", "zsh", "csh", "ksh", "fish", "run", "bin", "out", "elf", "so", "appimage", "deb", "rpm", "snap", "flatpak",
    "flatpakref", "desktop", "py", "pyc", "pyw", "pl", "rb", "php", "lua", "tcl", "jar", "jnlp", "class", "apk", "xpi",
    "crx",
    // Disk images and archives that mount or unpack into something runnable
    "iso", "img", "vhd", "vhdx",
    // Office documents with macros
    "docm", "dotm", "xlsm", "xltm", "xlam", "pptm", "potm", "ppam", "ppsm", "sldm",
];

/// MIME types that say "a program" whatever the name says.
const RUNS_CODE_MIME: &[&str] = &[
    "application/x-msdownload",
    "application/x-msdos-program",
    "application/x-ms-installer",
    "application/x-msi",
    "application/x-executable",
    "application/x-elf",
    "application/x-sharedlib",
    "application/x-mach-binary",
    "application/x-sh",
    "application/x-shellscript",
    "application/x-csh",
    "application/x-bat",
    "application/x-python",
    "application/x-perl",
    "application/x-ruby",
    "application/javascript",
    "text/javascript",
    "application/x-javascript",
    "application/java-archive",
    "application/x-java-jnlp-file",
    "application/vnd.android.package-archive",
    "application/x-apple-diskimage",
    "application/x-iso9660-image",
    "application/x-debian-package",
    "application/x-rpm",
    "application/x-desktop",
    "application/hta",
    "application/x-ms-shortcut",
    "application/vnd.ms-word.document.macroenabled.12",
    "application/vnd.ms-excel.sheet.macroenabled.12",
    "application/vnd.ms-powerpoint.presentation.macroenabled.12",
];

/// Why opening this attachment with the system's default program could run code, or None when it is a
/// document, a picture, an archive of files ... Both the name's last extension and the MIME type count
/// (a `.pdf.exe`, or a `.pdf` sent as `application/x-msdownload`). Clients offer "Save" instead of "Open"
/// for these, and never set an executable bit on anything from mail.
pub fn dangerous_to_open(filename: &str, mime: &str) -> Option<String> {
    let name = safe_file_name(filename);
    let ext = name.rsplit_once('.').map(|(_, e)| e.trim().to_ascii_lowercase()).unwrap_or_default();
    if !ext.is_empty() && RUNS_CODE.contains(&ext.as_str()) {
        return Some(format!("a .{ext} file can run code when it is opened"));
    }
    let mime = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    if RUNS_CODE_MIME.contains(&mime.as_str()) {
        return Some(format!("it is sent as a program ({mime})"));
    }
    None
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
        assert_eq!(safe_file_name("/"), "attachment");
        assert_eq!(safe_file_name("Q3 report.pdf"), "Q3 report.pdf");
        assert_eq!(safe_file_name(&"x".repeat(500)).len(), 120);
    }

    #[test]
    fn programs_are_dangerous_documents_are_not() {
        for n in ["setup.exe", "invoice.pdf.exe", "run.SH", "x.command", "macro.xlsm", "tool.AppImage", "a.desktop", "s.ps1"] {
            assert!(dangerous_to_open(n, "application/octet-stream").is_some(), "{n}");
        }
        for n in ["report.pdf", "photo.jpeg", "notes.txt", "data.csv", "slides.pptx", "archive.zip", "README", "a.exe.pdf"] {
            assert!(dangerous_to_open(n, "application/octet-stream").is_none(), "{n}");
        }
        assert!(dangerous_to_open("report.pdf", "application/x-msdownload").is_some());
        assert!(dangerous_to_open("report.pdf", "Application/X-SH; charset=utf-8").is_some());
        // A name that tries to hide the extension behind a path or trailing junk.
        assert!(dangerous_to_open("../evil.exe", "").is_some());
        assert!(dangerous_to_open("evil.exe ", "").is_some());
    }
}
