//! The Lumbridge Mail daemon. Runs in the foreground until SIGTERM / SIGINT; SIGHUP re-reads the config.
//!
//!     lumbridge-maild            # paths from LUMBRIDGE_MAIL_HOME or the XDG directories
//!     lumbridge-maild --version

use lumbridge_mail_core::paths::Paths;
use lumbridge_maild::{Daemon, Options};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("lumbridge-maild {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if let Some(a) = args.first() {
        eprintln!("lumbridge-maild: unknown argument {a:?} (it takes none; set LUMBRIDGE_MAIL_HOME to move its files)");
        std::process::exit(2);
    }
    let rt = match tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("lumbridge-maild: {e}");
            std::process::exit(1);
        }
    };
    let code = rt.block_on(async {
        let paths = Paths::from_env();
        match Daemon::start(Options::new(paths)).await {
            Ok(d) => {
                eprintln!("lumbridge-maild {}: listening on {}", env!("CARGO_PKG_VERSION"), d.socket().display());
                match d.run_until_signal().await {
                    Ok(()) => 0,
                    Err(e) => {
                        eprintln!("lumbridge-maild: {e}");
                        1
                    }
                }
            }
            Err(e) => {
                eprintln!("lumbridge-maild: {e}");
                1
            }
        }
    });
    std::process::exit(code);
}
