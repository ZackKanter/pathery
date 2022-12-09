use pathery::service::index::StatsIndexService;
use pathery::service::start_service;

#[tokio::main]
async fn main() -> Result<(), lambda_http::Error> {
    let service = StatsIndexService::create().await;

    start_service(&service).await
}
