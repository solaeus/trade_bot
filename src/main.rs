#![feature(duration_constructors)]

mod bot;
mod config;

use std::{env::var, fs::read_to_string};

use bot::Bot;
use config::{Config, Secrets};

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
    let game_server = config
        .game_server
        .unwrap_or("server.veloren.net".to_string());
    let auth_server = config
        .auth_server
        .unwrap_or("https://auth.veloren.net".to_string());
    let mut bot = Bot::new(
        game_server,
        &auth_server,
        secrets.username,
        &secrets.password,
        &secrets.character,
        secrets.admins,
        config.buy_prices,
        config.sell_prices,
        config.position,
        config.orientation,
        config.announcement,
    )
    .expect("Failed to create bot");

    loop {
        let _ = bot.tick().inspect_err(|error| log::error!("{error}"));
    }
}
