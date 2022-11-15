pub use lambda_http as http;
pub use lambda_runtime;
pub use tracing;
pub use tracing_subscriber;

pub use lambda_http::IntoResponse;
pub use lambda_http::RequestExt;

use aws_sdk_dynamodb::Client as DDBClient;
use std::sync::Arc;

pub async fn ddb_client() -> Arc<DDBClient> {
    let config = aws_config::load_from_env().await;
    Arc::new(aws_sdk_dynamodb::Client::new(&config))
}
