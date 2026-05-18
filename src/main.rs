#[tokio::main]
async fn main() -> std::process::ExitCode {
    match web_speedtest::app::serve().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            web_speedtest::app::exit_code_for_error(&error)
        }
    }
}
