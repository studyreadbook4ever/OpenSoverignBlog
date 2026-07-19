use osb_mcp::{Config, help_text, run_stdio};

#[tokio::main]
async fn main() {
    if std::env::args().any(|argument| argument == "-h" || argument == "--help") {
        print!("{}", help_text());
        return;
    }

    let config = match Config::from_process() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("osb-mcp configuration error: {error}");
            std::process::exit(2);
        }
    };

    if let Err(error) = run_stdio(config).await {
        eprintln!("osb-mcp stopped: {error}");
        std::process::exit(1);
    }
}
