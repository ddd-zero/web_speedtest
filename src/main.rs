#[tokio::main]
async fn main() -> std::process::ExitCode {
    match web_speed::app::serve().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            web_speed::app::exit_code_for_error(&error)
        }
    }
}
