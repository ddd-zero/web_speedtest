#[tokio::main]
async fn main() -> Result<(), web_speed::app::AppError> {
    web_speed::app::serve().await
}
