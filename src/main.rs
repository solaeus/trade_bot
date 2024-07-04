mod bot;

use std::{
    collections::HashMap,
    env::var,
    fs::{read_to_string, write},
    sync::{Arc, Mutex},
    thread::{sleep, spawn},
    time::Duration,
};

use bot::Bot;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Config {
    pub username: String,
    pub password: String,
    pub buy_prices: HashMap<String, u32>,
    pub sell_prices: HashMap<String, u32>,
    pub position: [f32; 3],
}

impl Config {
    fn read() -> Result<Self, String> {
        let config_path = var("CONFIG_PATH").map_err(|error| error.to_string())?;
        let config_file_content = read_to_string(config_path).map_err(|error| error.to_string())?;

        toml::from_str::<Config>(&config_file_content).map_err(|error| error.to_string())
    }

    fn write(&self) -> Result<(), String> {
        let config_path = var("CONFIG_PATH").map_err(|error| error.to_string())?;
        let config_string = toml::to_string(self).map_err(|error| error.to_string())?;

        write(config_path, config_string).map_err(|error| error.to_string())
    }
}

fn main() {
    let config = Config::read().unwrap();
    let mut bot = Bot::new(
        config.username,
        &config.password,
        config.buy_prices,
        config.sell_prices,
        config.position,
    )
    .expect("Failed to create bot");

    bot.select_character().expect("Failed to select character");

    loop {
        bot.tick().inspect_err(|error| eprintln!("{error}"));
    }
}
