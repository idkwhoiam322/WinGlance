#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod events;
mod logging;
mod overlay;
mod smtc;

use anyhow::Result;
use log::{error, info};
use std::sync::mpsc;
use std::thread;

fn main() -> Result<()> {
    let config = config::Config::load()?;
    logging::init_logging(&config.logs_dir(), config.logging.keep_files);
    info!("starting Notch");

    let (event_tx, event_rx) = mpsc::channel();
    let listener_config = config.clone();
    thread::Builder::new().name("notch-smtc".to_string()).spawn(move || {
        if let Err(error) = smtc::SmtcListener::new(event_tx, listener_config).run() {
            error!("SMTC listener stopped: {error:#}");
        }
    })?;

    overlay::run(config, event_rx)
}
