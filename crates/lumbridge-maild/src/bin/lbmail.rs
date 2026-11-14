//! `lbmail`: Lumbridge Mail from the command line. See `lbmail --help`.

use clap::Parser;

fn main() {
    let cli = lumbridge_maild::cli::Cli::parse();
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("lbmail: {e}");
            std::process::exit(1);
        }
    };
    std::process::exit(rt.block_on(lumbridge_maild::cli::run(cli)));
}
