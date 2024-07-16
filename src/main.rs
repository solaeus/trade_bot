#![feature(duration_constructors)]

mod bot;

use std::{borrow::Cow, env::var, fs::read_to_string, str::FromStr};

use bot::Bot;
use hashbrown::HashMap;
use serde::{de::Visitor, Deserialize};
use veloren_common::comp::item::{ItemDefinitionId, ItemDefinitionIdOwned, Material};

pub struct PriceList {
    pub simple: HashMap<ItemDefinitionIdOwned, u32>,
    pub modular: Vec<ModularItemPrice>,
}

impl<'de> Deserialize<'de> for PriceList {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(PriceListVisitor)
    }
}

pub struct PriceListVisitor;

impl<'de> Visitor<'de> for PriceListVisitor {
    type Value = PriceList;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a map with simple and/or modular keys")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        let mut simple = None;
        let mut modular = None;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "simple" => {
                    let simple_prices_with_item_string =
                        map.next_value::<HashMap<String, u32>>()?;
                    let simple_prices_with_item_id = simple_prices_with_item_string
                        .into_iter()
                        .map(|(mut key, value)| {
                            key.insert_str(0, "common.items.");

                            (ItemDefinitionIdOwned::Simple(key), value)
                        })
                        .collect();

                    simple = Some(simple_prices_with_item_id);
                }
                "modular" => {
                    modular = Some(map.next_value()?);
                }
                _ => {
                    return Err(serde::de::Error::unknown_field(
                        &key,
                        &["simple", "modular"],
                    ));
                }
            }
        }

        Ok(PriceList {
            simple: simple.ok_or_else(|| serde::de::Error::missing_field("simple"))?,
            modular: modular.ok_or_else(|| serde::de::Error::missing_field("modular"))?,
        })
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct ModularItemPrice {
    pub material: Material,
    pub primary: ItemDefinitionIdOwned,
    pub secondary: ItemDefinitionIdOwned,
    pub price: u32,
}

impl<'de> Deserialize<'de> for ModularItemPrice {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(ModularPriceVisitor)
    }
}

struct ModularPriceVisitor;

impl<'de> Visitor<'de> for ModularPriceVisitor {
    type Value = ModularItemPrice;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a map with material, primary, secondary and price keys")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        let mut material = None;
        let mut primary = None;
        let mut secondary = None;
        let mut price = None;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "material" => {
                    material = Some(map.next_value()?);
                }
                "primary" => {
                    let mut primary_string = map.next_value::<String>()?;

                    primary_string.insert_str(0, "common.items.modular.weapon.primary.");

                    primary = Some(ItemDefinitionIdOwned::Simple(primary_string));
                }
                "secondary" => {
                    let mut secondary_string = map.next_value::<String>()?;

                    secondary_string.insert_str(0, "common.items.modular.weapon.secondary.");

                    secondary = Some(ItemDefinitionIdOwned::Simple(secondary_string));
                }
                "price" => {
                    price = Some(map.next_value()?);
                }
                _ => {
                    return Err(serde::de::Error::unknown_field(
                        &key,
                        &["material", "primary", "secondary", "price"],
                    ));
                }
            }
        }

        Ok(ModularItemPrice {
            material: material.ok_or_else(|| serde::de::Error::missing_field("material"))?,
            primary: primary.ok_or_else(|| serde::de::Error::missing_field("primary"))?,
            secondary: secondary.ok_or_else(|| serde::de::Error::missing_field("secondary"))?,
            price: price.ok_or_else(|| serde::de::Error::missing_field("price"))?,
        })
    }
}

#[derive(Deserialize)]
struct Config {
    pub game_server: Option<String>,
    pub auth_server: Option<String>,
    pub position: Option<[f32; 3]>,
    pub orientation: Option<f32>,
    pub announcement: Option<String>,
    pub buy_prices: PriceList,
    pub sell_prices: PriceList,
}

#[derive(Deserialize)]
pub struct Secrets {
    pub username: String,
    pub password: String,
    pub character: String,
    pub admins: Vec<String>,
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
