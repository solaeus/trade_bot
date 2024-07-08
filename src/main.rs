mod bot;

use std::{
    collections::HashMap,
    env::{args, var},
    fs::read_to_string,
};

use bot::Bot;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct Secrets {
    pub username: String,
    pub password: String,
}

impl Secrets {
    fn read() -> Result<Self, String> {
        let config_path = var("SECRETS").map_err(|error| error.to_string())?;
        let config_file_content = read_to_string(config_path).map_err(|error| error.to_string())?;

        toml::from_str::<Secrets>(&config_file_content).map_err(|error| error.to_string())
    }
}

#[derive(Serialize, Deserialize)]
struct Config {
    pub buy_prices: HashMap<String, u32>,
    pub sell_prices: HashMap<String, u32>,
    pub position: [f32; 3],
    pub orientation: String,
}

impl Config {
    fn read() -> Result<Self, String> {
        let config_path = args()
            .nth(1)
            .expect("Pass an argument specifying the config file");
        let config_file_content = read_to_string(config_path).map_err(|error| error.to_string())?;

        toml::from_str::<Config>(&config_file_content).map_err(|error| error.to_string())
    }
}

fn main() {
    env_logger::init();

    let secrets = Secrets::read().unwrap();
    let config = Config::read().unwrap();
    let mut bot = Bot::new(
        &secrets.username,
        &secrets.password,
        config.buy_prices,
        config.sell_prices,
        config.position,
        config.orientation,
    )
    .expect("Failed to create bot");

    bot.select_character().expect("Failed to select character");

    loop {
        let _ = bot.tick().inspect_err(|error| eprintln!("{error}"));
    }
}
