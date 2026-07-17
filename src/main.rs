mod agent;
mod cli;
mod config;
mod daemon;
mod ipc;
mod model;
mod prompts;
mod store;
mod tools;
mod web;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    cli::run().await
}
