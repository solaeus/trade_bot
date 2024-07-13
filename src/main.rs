#![feature(duration_constructors)]

mod bot;

use std::{collections::HashMap, env::var, fs::read_to_string};

use bot::Bot;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct Secrets {
    pub username: String,
    pub password: String,
    pub character: String,
    pub admins: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct Config {
    pub position: [f32; 3],
    pub orientation: f32,
    pub announcement: Option<String>,
    pub buy_prices: HashMap<String, u32>,
    pub sell_prices: HashMap<String, u32>,
}

fn main() {
    env_logger::init();

    let secrets = {
        let secrets_path =
            var("SECRETS").expect("Provide a SECRETS variable specifying the secrets file");
        let file_content = read_to_string(secrets_path).expect("Failed to read secrets");

        toml::from_str::<Secrets>(&file_content).expect("Failed to parse secrets")
    };
    let config = {
        let config_path =
            var("CONFIG").expect("Provide a CONFIG variable specifying the config file");
        let file_content = read_to_string(config_path).expect("Failed to read config");

        toml::from_str::<Config>(&file_content).expect("Failed to parse config")
    };
    let buy_prices_with_full_id = config
        .buy_prices
        .into_iter()
        .map(|(mut item_id, price)| {
            item_id.insert_str(0, "common.items.");

            (item_id, price)
        })
        .collect();
    let sell_prices_with_full_id = config
        .sell_prices
        .into_iter()
        .map(|(mut item_id, price)| {
            item_id.insert_str(0, "common.items.");

            (item_id, price)
        })
        .collect();
    let mut bot = Bot::new(
        secrets.username,
        &secrets.password,
        &secrets.character,
        secrets.admins,
        buy_prices_with_full_id,
        sell_prices_with_full_id,
        config.position,
        config.orientation,
        config.announcement,
    )
    .expect("Failed to create bot");

    loop {
        let _ = bot.tick().inspect_err(|error| log::error!("{error}"));
    }
}
