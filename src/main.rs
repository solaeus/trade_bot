mod bot;

use std::{collections::HashMap, env::var, fs::read_to_string};

use bot::Bot;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct Secrets {
    pub username: String,
    pub password: String,
}

#[derive(Serialize, Deserialize)]
struct Config {
    pub character: String,
    pub buy_prices: HashMap<String, u32>,
    pub sell_prices: HashMap<String, u32>,
    pub position: [f32; 3],
    pub orientation: String,
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

    let mut bot = Bot::new(
        &secrets.username,
        &secrets.password,
        &config.character,
        config.buy_prices,
        config.sell_prices,
        config.position,
        config.orientation,
    )
    .expect("Failed to create bot");

    loop {
        let _ = bot.tick().inspect_err(|error| eprintln!("{error}"));
    }
}
