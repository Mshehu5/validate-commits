use clap::Parser;
use std::io;
use std::process::ExitCode;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Git revision range to validate
    #[arg(value_name = "REVISION-RANGE")]
    revision_range: String,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match validate_commits::run(&cli.revision_range, &mut io::stdout()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
