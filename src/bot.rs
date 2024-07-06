use std::{
    borrow::{Borrow, BorrowMut},
    collections::HashMap,
    sync::{
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    time::{self, Duration, Instant, UNIX_EPOCH},
};

use serde::Serialize;
use tokio::runtime::Runtime;
use veloren_client::{addr::ConnectionArgs, Client, Event as VelorenEvent, WorldExt};
use veloren_common::{
    clock::Clock,
    comp::{
        self,
        character_state::CharacterStateEventEmitters,
        invite::InviteKind,
        item::{self, ItemDefinitionId, ItemDefinitionIdOwned, ItemDesc},
        CharacterState, ChatType, Controller, ControllerInputs, InputKind, Item, Pos,
    },
    event::{EmitExt, EventBus},
    outcome::Outcome,
    trade::{PendingTrade, TradeAction, TradePhase},
    uid::Uid,
    ViewDistances,
};
use veloren_common_net::{
    msg::{InviteAnswer, Notification},
    sync::WorldSyncExt,
};

const COINS: &str = "common.items.utility.coins";

enum TradeMode {
    Take,
    Buy,
    Sell,
}

pub struct Bot {
    username: String,
    position: [f32; 3],
    client: Client,
    clock: Clock,
    buy_prices: HashMap<String, u32>,
    sell_prices: HashMap<String, u32>,
    last_action: Instant,
    last_announcement: Instant,
    trade_mode: TradeMode,
}

impl Bot {
    pub fn new(
        username: String,
        password: &str,
        buy_prices: HashMap<String, u32>,
        sell_prices: HashMap<String, u32>,
        position: [f32; 3],
    ) -> Result<Self, String> {
        let client = connect_to_veloren(&username, password)?;
        let clock = Clock::new(Duration::from_secs_f64(1.0 / 60.0));

        Ok(Bot {
            username,
            position,
            client,
            clock,
            buy_prices,
            sell_prices,
            last_action: Instant::now(),
            last_announcement: Instant::now(),
            trade_mode: TradeMode::Buy,
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
                terrain: 4,
                entity: 4,
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

        if self.last_action.elapsed() > Duration::from_millis(300) {
            if self.client.is_dead() {
                self.client.respawn();
            }

            if !self.client.is_lantern_enabled() {
                self.client.enable_lantern();
            }

            if let Some(position) = self.client.position() {
                if position != self.position.into() {
                    let entity = self.client.entity().clone();
                    let mut position_state = self.client.state_mut().ecs().write_storage::<Pos>();

                    position_state.insert(entity, Pos(self.position.into()));
                }
            }

            if let Some((_, trade, _)) = self.client.pending_trade() {
                match self.trade_mode {
                    TradeMode::Buy => self.handle_buy(trade.clone())?,
                    TradeMode::Take => self.handle_take(trade.clone()),
                    TradeMode::Sell => self.handle_sell(trade.clone())?,
                }
            }

            self.last_action = Instant::now();
        }

        if self.last_announcement.elapsed() > Duration::from_secs(600) {
            self.client.send_command(
                "region".to_string(),
                vec![
                    "I'm a bot. Use /say or /tell to give commands: 'buy', 'sell' or 'prices'."
                        .to_string(),
                ],
            );

            self.last_announcement = Instant::now();
        }

        self.clock.tick();

        Ok(())
    }

    fn handle_veloren_event(&mut self, event: VelorenEvent) -> Result<(), String> {
        match event {
            VelorenEvent::Chat(message) => {
                let content = message.content().as_plain().unwrap_or_default();

                match message.chat_type {
                    ChatType::Tell(sender_uid, _) | ChatType::Say(sender_uid) => {
                        if !self.client.is_trading() {
                            match content.trim() {
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
                        match content.trim() {
                            "prices" => {
                                let player_name = self
                                    .find_name(&sender_uid)
                                    .ok_or("Failed to find player name")?
                                    .to_string();

                                self.client.send_command(
                                    "tell".to_string(),
                                    vec![
                                        player_name.clone(),
                                        format!("Buy prices: {:?}", self.buy_prices),
                                    ],
                                );
                                self.client.send_command(
                                    "tell".to_string(),
                                    vec![
                                        player_name,
                                        format!("Sell prices: {:?}", self.sell_prices),
                                    ],
                                );
                            }
                            _ => {}
                        }
                    }

                    _ => {}
                }
            }
            VelorenEvent::Outcome(Outcome::ProjectileHit {
                pos,
                body,
                vel,
                source,
                target: Some(target),
            }) => {
                if let Some(uid) = self.client.uid() {
                    if uid == target {
                        self.client
                            .send_command("say".to_string(), vec!["Ouch!".to_string()])
                    }
                }
            }
            _ => (),
        }

        Ok(())
    }

    fn handle_buy(&mut self, trade: PendingTrade) -> Result<(), String> {
        let (my_offer, their_offer) = {
            let my_offer_index = trade
                .which_party(self.client.uid().ok_or("Failed to get uid")?)
                .ok_or("Failed to get offer index")?;
            let their_offer_index = if my_offer_index == 0 { 1 } else { 0 };

            (
                &trade.offers[my_offer_index],
                &trade.offers[their_offer_index],
            )
        };
        let inventories = self.client.inventories();
        let my_inventory = inventories.get(self.client.entity()).unwrap();
        let my_coins = my_inventory
            .get_slot_of_item_by_def_id(&ItemDefinitionIdOwned::Simple(COINS.to_string()))
            .ok_or("Failed to find coins".to_string())?;
        let them = self
            .client
            .state()
            .ecs()
            .entity_from_uid(trade.parties[1])
            .ok_or("Failed to find player".to_string())?;
        let their_inventory = inventories
            .get(them)
            .ok_or("Failed to find inventory".to_string())?;
        let their_offered_items_value =
            their_offer.into_iter().fold(0, |acc, (slot_id, quantity)| {
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
        let my_offered_coins = my_offer
            .into_iter()
            .find_map(|(slot_id, quantity)| {
                let item = if let Some(item) = my_inventory.get(slot_id.clone()) {
                    item
                } else {
                    return None;
                };

                if item.item_definition_id() == ItemDefinitionId::Simple(COINS.into()) {
                    Some(quantity)
                } else {
                    None
                }
            })
            .unwrap_or(&0);
        let difference: i32 = their_offered_items_value as i32 - *my_offered_coins as i32;

        let mut my_items_to_remove = Vec::new();

        for (item_id, quantity) in my_offer {
            let item = my_inventory
                .get(item_id.clone())
                .ok_or("Failed to find item".to_string())?;

            if item.item_definition_id() != ItemDefinitionId::Simple(COINS.into()) {
                my_items_to_remove.push((item_id.clone(), *quantity));
            }
        }

        let mut their_items_to_remove = Vec::new();

        for (item_id, quantity) in their_offer {
            let item = their_inventory
                .get(item_id.clone())
                .ok_or("Failed to find item".to_string())?;

            if !self.buy_prices.contains_key(&item.persistence_item_id()) {
                their_items_to_remove.push((item_id.clone(), *quantity));
            }
        }

        drop(inventories);

        for (item, quantity) in my_items_to_remove {
            self.client.perform_trade_action(TradeAction::RemoveItem {
                item,
                quantity,
                ours: true,
            });
        }

        for (item, quantity) in their_items_to_remove {
            self.client.perform_trade_action(TradeAction::RemoveItem {
                item,
                quantity,
                ours: false,
            });
        }

        if difference == 0 {
            self.client
                .perform_trade_action(TradeAction::Accept(trade.phase));
        } else if difference.is_positive() {
            self.client.perform_trade_action(TradeAction::AddItem {
                item: my_coins,
                quantity: difference as u32,
                ours: true,
            });
        } else if difference.is_negative() {
            self.client.perform_trade_action(TradeAction::RemoveItem {
                item: my_coins,
                quantity: difference.abs() as u32,
                ours: true,
            });
        }

        Ok(())
    }

    fn handle_sell(&mut self, trade: PendingTrade) -> Result<(), String> {
        let (my_offer, their_offer) = {
            let my_offer_index = trade
                .which_party(self.client.uid().ok_or("Failed to get uid")?)
                .ok_or("Failed to get offer index")?;
            let their_offer_index = if my_offer_index == 0 { 1 } else { 0 };

            (
                &trade.offers[my_offer_index],
                &trade.offers[their_offer_index],
            )
        };
        let inventories = self.client.inventories();
        let my_inventory = inventories.get(self.client.entity()).unwrap();
        let them = self
            .client
            .state()
            .ecs()
            .entity_from_uid(trade.parties[1])
            .ok_or("Failed to find player".to_string())?;
        let their_inventory = inventories
            .get(them)
            .ok_or("Failed to find inventory".to_string())?;
        let their_coins = their_inventory
            .get_slot_of_item_by_def_id(&ItemDefinitionIdOwned::Simple(COINS.to_string()))
            .ok_or("Failed to find coins")?;
        let my_offered_items_value = my_offer.into_iter().fold(0, |acc, (slot_id, quantity)| {
            if let Some(item) = my_inventory.get(slot_id.clone()) {
                let item_value = self
                    .sell_prices
                    .get(&item.persistence_item_id())
                    .unwrap_or(&0);

                acc + (item_value * quantity)
            } else {
                acc
            }
        });
        let their_offered_coins = their_offer
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

        let mut their_items_to_remove = Vec::new();

        for (item_id, quantity) in their_offer {
            let item = their_inventory
                .get(item_id.clone())
                .ok_or("Failed to find item".to_string())?;

            if item.item_definition_id()
                != ItemDefinitionId::Simple("common.items.utility.coins".into())
            {
                their_items_to_remove.push((item_id.clone(), *quantity));
            }
        }

        let mut my_items_to_remove = Vec::new();

        for (item_id, quantity) in my_offer {
            let item = my_inventory
                .get(item_id.clone())
                .ok_or("Failed to find item".to_string())?;

            if !self.sell_prices.contains_key(&item.persistence_item_id()) {
                my_items_to_remove.push((item_id.clone(), *quantity));
            }
        }

        drop(inventories);

        for (item, quantity) in their_items_to_remove {
            self.client.perform_trade_action(TradeAction::RemoveItem {
                item,
                quantity,
                ours: false,
            });
        }

        for (item, quantity) in my_items_to_remove {
            self.client.perform_trade_action(TradeAction::RemoveItem {
                item,
                quantity,
                ours: true,
            });
        }

        if difference == 0 {
            self.client
                .perform_trade_action(TradeAction::Accept(trade.phase));
        } else if difference.is_positive() {
            self.client.perform_trade_action(TradeAction::AddItem {
                item: their_coins,
                quantity: difference as u32,
                ours: false,
            });
        } else if difference.is_negative() {
            self.client.perform_trade_action(TradeAction::RemoveItem {
                item: their_coins,
                quantity: difference.abs() as u32,
                ours: false,
            });
        }

        Ok(())
    }

    fn handle_take(&mut self, trade: PendingTrade) {
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
