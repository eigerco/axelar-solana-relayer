use clap::{Parser, Subcommand};
use xshell::{cmd, Shell};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Deny {
        #[clap(last = true)]
        args: Vec<String>,
    },
    Test {
        #[clap(short, long, default_value_t = false)]
        coverage: bool,
        #[clap(last = true)]
        args: Vec<String>,
    },
    Check,
    Fmt,
    Doc,
    UnusedDeps,
}

fn main() -> eyre::Result<()> {
    color_eyre::install()?;
    let sh = Shell::new()?;
    let args = Args::parse();

    match args.command {
        Commands::Deny { args } => {
            println!("cargo deny");
            cmd!(sh, "cargo +nightly install cargo-deny").run()?;
            cmd!(sh, "cargo deny check {args...}").run()?;
        }
        Commands::Test { args, coverage } => {
            println!("cargo test");
            cmd!(sh, "cargo install cargo-nextest --version 0.9.96 --locked").run()?;

            if coverage {
                cmd!(sh, "cargo install grcov").run()?;
                for (key, val) in [
                    ("CARGO_INCREMENTAL", "0"),
                    ("RUSTFLAGS", "-Cinstrument-coverage"),
                    ("LLVM_PROFILE_FILE", "target/coverage/%p-%m.profraw"),
                ] {
                    sh.set_var(key, val);
                }
            }
            cmd!(
                sh,
                "cargo nextest run --workspace --tests --all-targets --no-fail-fast {args...}"
            )
            .run()?;

            if coverage {
                cmd!(sh, "mkdir -p target/coverage").run()?;
                cmd!(sh, "grcov . --binary-path ./target/debug/deps/ -s . -t html,cobertura --branch --ignore-not-existing --ignore '../*' --ignore \"/*\" -o target/coverage/").run()?;

                // Open the generated file
                if std::option_env!("CI").is_none() {
                    #[cfg(target_os = "macos")]
                    cmd!(sh, "open target/coverage/html/index.html").run()?;

                    #[cfg(target_os = "linux")]
                    cmd!(sh, "xdg-open target/coverage/html/index.html").run()?;
                }
            }
        }

        Commands::Check => {
            println!("cargo check");
            cmd!(sh, "cargo clippy --workspace --locked -- -D warnings").run()?;
            cmd!(sh, "cargo fmt --all --check").run()?;
        }
        Commands::Fmt => {
            println!("cargo fix");
            cmd!(sh, "cargo fmt --all").run()?;
            cmd!(
                sh,
                "cargo fix --allow-dirty --allow-staged --workspace --tests"
            )
            .run()?;
            cmd!(
                sh,
                "cargo clippy --fix --allow-dirty --allow-staged --workspace --tests"
            )
            .run()?;
        }
        Commands::Doc => {
            println!("cargo doc");
            cmd!(sh, "cargo doc --workspace --no-deps").run()?;

            if std::option_env!("CI").is_none() {
                #[cfg(target_os = "macos")]
                cmd!(sh, "open target/doc/relayer/index.html").run()?;

                #[cfg(target_os = "linux")]
                cmd!(sh, "xdg-open target/doc/relayer/index.html").run()?;
            }
        }
        Commands::UnusedDeps => {
            println!("unused deps");
            cmd!(sh, "cargo +nightly install cargo-machete").run()?;
            cmd!(sh, "cargo-machete").run()?;
        }
    }

    Ok(())
}
