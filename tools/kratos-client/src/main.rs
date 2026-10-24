mod commands;

use std::process::ExitCode;

use commands::Failure;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match commands::run(&arguments) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Usage(message)) => {
            eprintln!("{message}\n\n{}", commands::USAGE);
            ExitCode::from(2)
        }
        Err(Failure::Client(error)) => {
            eprintln!("{}", serde_json::json!({ "error": error.to_string() }));
            ExitCode::FAILURE
        }
    }
}
