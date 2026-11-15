use aiom::cli::{self, Cli};
use aiom::ctx::Ctx;
use clap::Parser;

fn main() {
    let cli = Cli::parse();
    let ctx = match Ctx::from_env() {
        Ok(ctx) => ctx,
        Err(err) => {
            eprintln!("error: {err:#}");
            std::process::exit(1);
        }
    };
    if let Err(err) = cli::run(cli, &ctx) {
        eprintln!("error: {}", ctx.tilde(&format!("{err:#}")));
        std::process::exit(1);
    }
}
