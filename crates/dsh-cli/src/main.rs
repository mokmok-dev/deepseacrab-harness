use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    match dsh_cli::run().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("dsh: {error:#}");
            ExitCode::FAILURE
        }
    }
}
