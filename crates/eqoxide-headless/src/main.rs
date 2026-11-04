use eqoxide_headless::{profile::Profile, runtime};
#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() != 3 || args[1] != "--profile" {
        eprintln!("Usage: eqoxide-headless --profile /absolute/private-profile.json");
        std::process::exit(2);
    }
    let result = match Profile::load(std::path::Path::new(&args[2])) {
        Ok(profile) => runtime::run(profile).await,
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        eprintln!("headless: {error}");
        std::process::exit(1);
    }
}
