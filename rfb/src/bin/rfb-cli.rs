use clap::Parser;
use rfb::cli::commands::Cli;
use rfb::cli::dispatch;
use rfb::cli::error::render_error;

fn main() {
    let cli = Cli::parse();
    let json = cli.json;
    if let Err(error) = dispatch::run(cli) {
        render_error(json, &error);
        std::process::exit(error.code);
    }
}
