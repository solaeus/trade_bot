use std::{
    borrow::Borrow,
    collections::HashMap,
    sync::{
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use serde::Serialize;
use tokio::runtime::Runtime;
use veloren_client::{addr::ConnectionArgs, Client, Event as VelorenEvent};
use veloren_common::{
    clock::Clock,
    comp::{
        invite::InviteKind,
        item::{self, ItemDefinitionId, ItemDefinitionIdOwned, ItemDesc},
        ChatType, ControllerInputs, Item,
    },
    trade::{PendingTrade, TradeAction, TradePhase},
    uid::Uid,
    ViewDistances,
};
use veloren_common_net::{msg::InviteAnswer, sync::WorldSyncExt};

const COINS: &str = "common.items.utility.coins";

enum TradeMode {
    Take,
    Buy,
    Sell,
}

pub struct Bot {
    username: String,
    client: Client,
    clock: Clock,
    buy_prices: HashMap<String, u32>,
    sell_prices: HashMap<String, u32>,
    last_action: Instant,
    trade_mode: TradeMode,
}

impl Bot {
    pub fn new(
        username: String,
        password: &str,
        buy_prices: HashMap<String, u32>,
        sell_prices: HashMap<String, u32>,
    ) -> Result<Self, String> {
        let client = connect_to_veloren(&username, password)?;
        let clock = Clock::new(Duration::from_secs_f64(1.0 / 60.0));

        Ok(Bot {
            username,
            client,
            clock,
            buy_prices,
            sell_prices,
            last_action: Instant::from(0),
            trade_mode: TradeMode::Sell,
        })
    }

    pub fn select_character(&mut self) -> Result<(), String> {
        self.client.load_character_list();

        while self.client.character_list().loading {
            self.client
                .tick(ControllerInputs::default(), self.clock.dt())
                .map_err(|error| format!("{error:?}"))?;
            self.clock.tick();
        }

        let character_id = self
            .client
            .character_list()
            .characters
            .first()
            .expect("No characters to select")
            .character
            .id
            .expect("Failed to get character ID");

        self.client.request_character(
            character_id,
            ViewDistances {
                terrain: 0,
                entity: 0,
            },
        );

        Ok(())
    }

    pub fn tick(&mut self) -> Result<(), String> {
        let veloren_events = self
            .client
            .tick(ControllerInputs::default(), self.clock.dt())
            .map_err(|error| format!("{error:?}"))?;

        for event in veloren_events {
            self.handle_veloren_event(event)?;
        }

        if self.last_action.elapsed() > Duration::from_millis(100) {
            if let Some((_, trade, _)) = self.client.pending_trade() {
                match self.trade_mode {
                    TradeMode::Buy => self.handle_buying(trade)?,
                    TradeMode::Take => self.handle_take(trade),
                    TradeMode::Sell => self.handle_selling(trade)?,
                }
            }
        }

        self.client.cleanup();
        self.clock.tick();

        Ok(())
    }

    fn handle_veloren_event(&mut self, event: VelorenEvent) -> Result<(), String> {
        if let VelorenEvent::Chat(message) = event {
            let content = message.content().as_plain().unwrap_or_default();

            if !content.starts_with(&self.username) {
                return Ok(());
            }

            match message.chat_type {
                ChatType::Tell(sender_uid, _) | ChatType::Say(sender_uid) => {
                    if !self.client.is_trading() {
                        match content.trim_start_matches(&self.username).trim() {
                            "buy" => {
                                self.trade_mode = TradeMode::Buy;
                                self.client.send_invite(sender_uid, InviteKind::Trade);
                            }
                            "sell" => {
                                self.trade_mode = TradeMode::Sell;
                                self.client.send_invite(sender_uid, InviteKind::Trade);
                            }
                            "take" => {
                                self.trade_mode = TradeMode::Take;
                                self.client.send_invite(sender_uid, InviteKind::Trade);
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn handle_buying(&mut self, trade: &PendingTrade) -> Result<(), String> {
        let inventories = self.client.inventories();
        let my_inventory = inventories.get(self.client.entity()).unwrap();
        let my_coins = my_inventory.get_slot_of_item_by_def_id(&ItemDefinitionIdOwned::Simple(
            "common.items.utility.coins".to_string(),
        ));
        let them = self
            .client
            .state()
            .ecs()
            .entity_from_uid(trade.parties[1])
            .unwrap();
        let their_inventory = inventories.get(them).unwrap();
        let their_offered_items_value =
            (&trade.offers[1])
                .into_iter()
                .fold(0, |acc, (slot_id, quantity)| {
                    if let Some(item) = their_inventory.get(slot_id.clone()) {
                        let item_value = self
                            .buy_prices
                            .get(&item.persistence_item_id())
                            .unwrap_or(&1);

                        acc + (item_value * quantity)
                    } else {
                        acc
                    }
                });
        let my_offered_coins = (&trade.offers[0])
            .into_iter()
            .find_map(|(slot_id, quantity)| {
                let item = if let Some(item) = my_inventory.get(slot_id.clone()) {
                    item
                } else {
                    return None;
                };

                if item.item_definition_id()
                    == ItemDefinitionId::Simple("common.items.utility.coins".into())
                {
                    Some(quantity)
                } else {
                    None
                }
            })
            .unwrap_or(&0);
        let difference: i32 = their_offered_items_value as i32 - *my_offered_coins as i32;

        drop(inventories);

        if difference == 0 {
            self.client
                .perform_trade_action(TradeAction::Accept(trade.phase));
        } else if difference.is_positive() {
            self.client.perform_trade_action(TradeAction::AddItem {
                item: my_coins.unwrap(),
                quantity: difference as u32,
                ours: true,
            });
        } else if difference.is_negative() {
            self.client.perform_trade_action(TradeAction::RemoveItem {
                item: my_coins.unwrap(),
                quantity: difference.abs() as u32,
                ours: true,
            });
        }

        Ok(())
    }

    fn handle_selling(&mut self, trade: &PendingTrade) -> Result<(), String> {
        let inventories = self.client.inventories();
        let my_inventory = inventories.get(self.client.entity()).unwrap();
        let them = self
            .client
            .state()
            .ecs()
            .entity_from_uid(trade.parties[1])
            .unwrap();
        let their_inventory = inventories.get(them).unwrap();
        let their_coins = their_inventory.get_slot_of_item_by_def_id(
            &ItemDefinitionIdOwned::Simple("common.items.utility.coins".to_string()),
        );
        let my_offered_items_value =
            (&trade.offers[0])
                .into_iter()
                .fold(0, |acc, (slot_id, quantity)| {
                    if let Some(item) = my_inventory.get(slot_id.clone()) {
                        println!("{}", item.persistence_item_id());

                        let item_value = self
                            .sell_prices
                            .get(&item.persistence_item_id())
                            .unwrap_or(&0);

                        acc + (item_value * quantity)
                    } else {
                        acc
                    }
                });
        let their_offered_coins = (&trade.offers[1])
            .into_iter()
            .find_map(|(slot_id, quantity)| {
                let item = if let Some(item) = their_inventory.get(slot_id.clone()) {
                    item
                } else {
                    return None;
                };

                if item.item_definition_id()
                    == ItemDefinitionId::Simple("common.items.utility.coins".into())
                {
                    Some(quantity)
                } else {
                    None
                }
            })
            .unwrap_or(&0);
        let difference: i32 = my_offered_items_value as i32 - *their_offered_coins as i32;

        drop(inventories);

        if difference == 0 {
            self.client
                .perform_trade_action(TradeAction::Accept(trade.phase));
        } else if difference.is_positive() {
            self.client.perform_trade_action(TradeAction::AddItem {
                item: their_coins.unwrap(),
                quantity: difference as u32,
                ours: false,
            });
        } else if difference.is_negative() {
            self.client.perform_trade_action(TradeAction::RemoveItem {
                item: their_coins.unwrap(),
                quantity: difference.abs() as u32,
                ours: false,
            });
        }

        Ok(())
    }

    fn handle_take(&self, trade: &PendingTrade) {
        if trade.offers[0].is_empty() && !trade.offers[1].is_empty() {
            self.client
                .perform_trade_action(TradeAction::Accept(trade.phase));
        }
    }

    fn find_name<'a>(&'a self, uid: &Uid) -> Option<&'a String> {
        self.client.player_list().iter().find_map(|(id, info)| {
            if id == uid {
                if let Some(character_info) = &info.character {
                    return Some(&character_info.name);
                }
            }
            None
        })
    }

    fn find_uid<'a>(&'a self, name: &str) -> Option<&'a Uid> {
        self.client.player_list().iter().find_map(|(id, info)| {
            if info.player_alias == name {
                Some(id)
            } else {
                None
            }
        })
    }

    fn find_uuid(&self, name: &str) -> Option<String> {
        self.client.player_list().iter().find_map(|(_, info)| {
            if info.player_alias == name {
                Some(info.uuid.to_string())
            } else {
                None
            }
        })
    }
}

fn connect_to_veloren(username: &str, password: &str) -> Result<Client, String> {
    let runtime = Arc::new(Runtime::new().unwrap());
    let runtime2 = Arc::clone(&runtime);

    runtime
        .block_on(Client::new(
            ConnectionArgs::Tcp {
                hostname: "server.veloren.net".to_string(),
                prefer_ipv6: false,
            },
            runtime2,
            &mut None,
            username,
            password,
            None,
            |provider| provider == "https://auth.veloren.net",
            &|_| {},
            |_| {},
            Default::default(),
        ))
        .map_err(|error| format!("{error:?}"))
}
