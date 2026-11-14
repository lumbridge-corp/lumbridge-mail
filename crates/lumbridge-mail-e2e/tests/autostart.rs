//! The app's `Rpc` starts the real `lumbridge-maild` binary the way an install does (no arguments, the
//! environment inherited), on a home deep enough that the socket needs the short `/tmp` fallback, and
//! the app, the daemon and the token file all agree on where things are.
//!
//! Needs the `lumbridge-maild` binary built next to this test (`cargo test --workspace` builds it);
//! without it the test says so and stops.

use lumbridge_mail_app::backend::rpc::{find_daemon, Autostart, Rpc};
use lumbridge_mail_app::backend::{Backend, LinkState};
use lumbridge_mail_core::paths::{Paths, MAX_SOCKET_PATH};
use std::time::Duration;

const CONFIG: &str = r#"
[ai]
enabled = false

[[accounts]]
id = "personal"
persona = "personal"
email = "sam@example.com"
name = "Sam Example"
[accounts.provider]
kind = "demo"

[[accounts]]
id = "anon"
persona = "anon"
email = "otter@anon.example"
[accounts.provider]
kind = "demo"
"#;

fn main() {
    let exe = std::env::current_exe().unwrap();
    let bin = exe.parent().and_then(|d| d.parent()).map(|d| d.join("lumbridge-maild")).unwrap();
    if !bin.is_file() {
        println!("autostart: SKIPPED, {} is not built (run cargo test --workspace)", bin.display());
        return;
    }
    // Next to the app binary wins, as in an install (bin/ or the .app's Contents/MacOS/).
    assert_eq!(find_daemon(Some(&bin.with_file_name("lumbridge-mail")), None).as_deref(), Some(bin.as_path()));

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join(format!("a-home-deep-enough-that-maild-sock-overflows-sun-path-{}", "x".repeat(40)));
    std::fs::create_dir_all(home.join("config")).unwrap();
    std::fs::write(home.join("config/config.toml"), CONFIG).unwrap();
    // SAFETY: first thing in a single-threaded test binary, before any thread exists.
    unsafe { std::env::set_var("LUMBRIDGE_MAIL_HOME", &home) };
    let paths = Paths::from_env();
    assert!(home.join("maild.sock").as_os_str().len() > MAX_SOCKET_PATH);
    assert!(paths.socket().starts_with("/tmp"), "{}", paths.socket().display());
    assert_eq!(lumbridge_mail_app::paths::socket(), paths.socket(), "the app computes the daemon's path");

    let rpc = Rpc::with_autostart(paths.socket(), Autostart::Program(bin.clone()));
    let link = rpc.link().unwrap();
    let hello = futures::executor::block_on(rpc.hello()).expect("the app started the daemon and said hello");
    assert!(hello.person, "the token next to the fallback socket was found and accepted");
    let accounts = futures::executor::block_on(rpc.accounts()).unwrap();
    assert_eq!(accounts.len(), 2);
    let mut seen = Vec::new();
    while let Ok(s) = link.try_recv() {
        seen.push(s);
    }
    assert!(seen.iter().any(|s| matches!(s, LinkState::Starting { .. })), "{seen:?}");
    let log = std::fs::read_to_string(paths.data_dir.join("maild.log")).unwrap_or_default();
    assert!(log.contains("listening on"), "the daemon's output goes to its log: {log:?}");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(paths.data_dir.join("maild.log")).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the log is private");
    println!("ok  autostart of the real lumbridge-maild (no arguments), fallback socket {}", paths.socket().display());

    // Stop it (it runs in its own process group, so it would outlive the test).
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let pid = lumbridge_mail_e2e::socket_peer_pid(&paths.socket()).expect("daemon pid");
        // SAFETY: plain kill(2) on the pid we just read from the socket.
        unsafe { libc::kill(pid, libc::SIGTERM) };
        lumbridge_mail_e2e::wait_real("the daemon to exit", Duration::from_secs(10), || !paths.socket().exists());
    }
    let _ = std::fs::remove_dir_all(paths.socket_dir());
    println!("autostart: ok");
}
