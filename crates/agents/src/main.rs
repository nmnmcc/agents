mod cli;
mod config;
mod daemon;
mod harness;
mod mail;
mod service;
mod types;

fn main() -> std::process::ExitCode {
    cli::main()
}
