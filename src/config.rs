/// Configuration used to initiate the bot.
///
/// The Config struct is used to store configuration values that are not sensitive. This includes
/// the price data for items, the game server to connect to, and the position and orientation of
/// the bot. The price lists have manual implementations for deserialization to allow turning
/// shortened item IDs into the full IDs used by the Veloren client.
///
/// The Secrets struct is used to store sensitive information that should not be shared. This
/// should be read from a separate file that is not checked into version control. In production,
/// use a secure means of storing this information, such as the secret manager for Podman.
use hashbrown::HashMap;
use serde::{
    de::{self, Visitor},
    Deserialize,
};
use veloren_common::comp::item::ItemDefinitionIdOwned;

#[derive(Deserialize)]
/// Non-sensitive configuration values.
///
/// See the [module-level documentation](index.html) for more information.
pub struct Config {
    pub game_server: Option<String>,
    pub auth_server: Option<String>,
    pub position: Option<[f32; 3]>,
    pub orientation: Option<f32>,
    pub announcement: Option<String>,
    pub buy_prices: PriceList,
    pub sell_prices: PriceList,
}

#[derive(Deserialize)]
/// Sensitive configuration values.
///
/// See the [module-level documentation](index.html) for more information.
pub struct Secrets {
    pub username: String,
    pub password: String,
    pub character: String,
    pub admins: Vec<String>,
}

/// Buy or sell prices for items.
pub struct PriceList(pub HashMap<ItemDefinitionIdOwned, u32>);

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
        formatter.write_str("a map with simple and/or modular keys: material|primary|secondary")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        let mut prices = HashMap::new();

        while let Some((key, value)) = map.next_entry::<String, u32>()? {
            let item_id = match key.splitn(3, '|').collect::<Vec<&str>>().as_slice() {
                [material, primary, secondary] => {
                    let material = "common.items.".to_string() + material;
                    let primary = "common.items.modular.weapon.primary.".to_string() + primary;
                    let secondary =
                        "common.items.modular.weapon.secondary.".to_string() + secondary;

                    ItemDefinitionIdOwned::Modular {
                        pseudo_base: "veloren.core.pseudo_items.modular.tool".to_string(),
                        components: vec![
                            ItemDefinitionIdOwned::Compound {
                                simple_base: primary,
                                components: vec![ItemDefinitionIdOwned::Simple(material)],
                            },
                            ItemDefinitionIdOwned::Simple(secondary),
                        ],
                    }
                }
                [simple] => {
                    let mut simple = simple.to_string();

                    simple.insert_str(0, "common.items.");

                    ItemDefinitionIdOwned::Simple(simple)
                }
                _ => return Err(de::Error::custom("Invalid key format")),
            };

            prices.insert(item_id, value);
        }

        Ok(PriceList(prices))
    }
}
