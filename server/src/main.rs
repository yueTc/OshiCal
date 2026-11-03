use oshical_server::config::ServerConfig;
use oshical_server::run_server;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("oshical_server=info,tower_http=info")),
        )
        .init();
    let config = match ServerConfig::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("配置错误: {error}");
            std::process::exit(2);
        }
    };
    if let Err(error) = run_server(config).await {
        eprintln!("服务器失败: {error}");
        std::process::exit(1);
    }
}
