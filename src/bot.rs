use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::runtime::Runtime;
use vek::{num_traits::Float, Quaternion};
use veloren_client::{addr::ConnectionArgs, Client, Event as VelorenEvent, WorldExt};
use veloren_common::{
    clock::Clock,
    comp::{invite::InviteKind, item::ItemDefinitionIdOwned, ChatType, ControllerInputs, Ori, Pos},
    outcome::Outcome,
    time::DayPeriod,
    trade::{PendingTrade, TradeAction},
    uid::Uid,
    ViewDistances,
};
use veloren_common_net::sync::WorldSyncExt;

const COINS: &str = "common.items.utility.coins";

pub struct Bot {
    position: [f32; 3],
    orientation: String,

    client: Client,
    clock: Clock,

    buy_prices: HashMap<String, u32>,
    sell_prices: HashMap<String, u32>,
    trade_mode: TradeMode,

    is_player_notified: bool,
    last_action: Instant,
    last_announcement: Instant,
    last_ouch: Instant,
}

impl Bot {
    pub fn new(
        username: &str,
        password: &str,
        buy_prices: HashMap<String, u32>,
        sell_prices: HashMap<String, u32>,
        position: [f32; 3],
        orientation: String,
    ) -> Result<Self, String> {
        log::info!("Connecting to veloren");

        let client = connect_to_veloren(username, password)?;
        let clock = Clock::new(Duration::from_secs_f64(1.0 / 30.0));
        let now = Instant::now();

        Ok(Bot {
            position,
            orientation,
            client,
            clock,
            buy_prices,
            sell_prices,
            trade_mode: TradeMode::Trade,
            is_player_notified: false,
            last_action: now,
            last_announcement: now,
            last_ouch: now,
        })
    }

    pub fn select_character(&mut self) -> Result<(), String> {
        log::info!("Selecting a character");

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

            self.handle_position_and_orientation()?;
            self.handle_lantern();

            if let Some((_, trade, _)) = self.client.pending_trade() {
                match self.trade_mode {
                    TradeMode::Trade => self.handle_trade(trade.clone())?,
                    TradeMode::Take => self.handle_take(trade.clone())?,
                }
            } else if self.client.pending_invites().is_empty() {
                self.is_player_notified = false;

                self.client.accept_invite();
            }

            self.last_action = Instant::now();

            if self.last_announcement.elapsed() > Duration::from_secs(1200) {
                self.client.send_command(
                    "region".to_string(),
                    vec!["I'm a bot. Trade with me or say 'prices' to see my offers.".to_string()],
                );

                self.last_announcement = Instant::now();
            }
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
                        match content.trim() {
                            "prices" => self.send_price_info(&sender_uid)?,
                            "take" => {
                                if !self.client.is_trading() {
                                    self.trade_mode = TradeMode::Take;
                                    self.client.send_invite(sender_uid, InviteKind::Trade);
                                }
                            }
                            _ => {}
                        }
                    }

                    _ => {}
                }
            }
            VelorenEvent::Outcome(Outcome::ProjectileHit {
                target: Some(target),
                ..
            }) => {
                if let Some(uid) = self.client.uid() {
                    if uid == target && self.last_ouch.elapsed() > Duration::from_secs(1) {
                        self.client
                            .send_command("say".to_string(), vec!["Ouch!".to_string()]);

                        self.last_ouch = Instant::now();
                    }
                }
            }
            VelorenEvent::TradeComplete { result, .. } => {
                log::info!("Completed trade: {result:?}");

                if let TradeMode::Take = self.trade_mode {
                    self.trade_mode = TradeMode::Trade
                }
            }
            _ => (),
        }

        Ok(())
    }

    fn handle_lantern(&mut self) {
        let day_period = self.client.state().get_day_period();

        match day_period {
            DayPeriod::Night => {
                if !self.client.is_lantern_enabled() {
                    self.client.enable_lantern();
                }
            }
            DayPeriod::Morning | DayPeriod::Noon | DayPeriod::Evening => {
                if self.client.is_lantern_enabled() {
                    self.client.disable_lantern();
                }
            }
        }
    }

    fn handle_trade(&mut self, trade: PendingTrade) -> Result<(), String> {
        let my_offer_index = trade
            .which_party(self.client.uid().ok_or("Failed to get uid")?)
            .ok_or("Failed to get offer index")?;
        let their_offer_index = if my_offer_index == 0 { 1 } else { 0 };
        let (my_offer, their_offer, them) = {
            (
                &trade.offers[my_offer_index],
                &trade.offers[their_offer_index],
                self.client
                    .state()
                    .ecs()
                    .entity_from_uid(trade.parties[their_offer_index])
                    .ok_or("Failed to find player".to_string())?,
            )
        };
        let inventories = self.client.inventories();
        let my_inventory = inventories.get(self.client.entity()).unwrap();
        let my_coins = my_inventory
            .get_slot_of_item_by_def_id(&ItemDefinitionIdOwned::Simple(COINS.to_string()))
            .ok_or("Failed to find coins".to_string())?;
        let their_inventory = inventories
            .get(them)
            .ok_or("Failed to find inventory".to_string())?;
        let their_coins = their_inventory
            .get_slot_of_item_by_def_id(&ItemDefinitionIdOwned::Simple(COINS.to_string()))
            .ok_or("Failed to find coins")?;
        let their_total_coin_amount = their_inventory
            .get(their_coins)
            .map(|item| item.amount() as i32)
            .unwrap_or(0);
        let (mut their_offered_coin_amount, mut my_offered_coin_amount) = (0, 0);
        let their_offered_items_value =
            their_offer
                .into_iter()
                .fold(0, |acc: i32, (slot_id, quantity)| {
                    if let Some(item) = their_inventory.get(*slot_id) {
                        let item_id = item.persistence_item_id();

                        let item_value = if item_id == COINS {
                            their_offered_coin_amount = *quantity as i32;

                            1
                        } else {
                            self.buy_prices
                                .get(&item_id)
                                .map(|int| *int as i32)
                                .unwrap_or_else(|| {
                                    self.sell_prices
                                        .get(&item_id)
                                        .map(|int| 0 - *int as i32)
                                        .unwrap_or(0)
                                })
                        };

                        acc.saturating_add(item_value.saturating_mul(*quantity as i32))
                    } else {
                        acc
                    }
                });
        let my_offered_items_value =
            my_offer
                .into_iter()
                .fold(0, |acc: i32, (slot_id, quantity)| {
                    if let Some(item) = my_inventory.get(*slot_id) {
                        let item_id = item.persistence_item_id();

                        let item_value = if item_id == COINS {
                            my_offered_coin_amount = *quantity as i32;

                            1
                        } else {
                            self.sell_prices
                                .get(&item_id)
                                .map(|int| *int as i32)
                                .unwrap_or_else(|| {
                                    self.buy_prices
                                        .get(&item_id)
                                        .map(|int| 0 - *int as i32)
                                        .unwrap_or(i32::MIN)
                                })
                        };

                        acc.saturating_add(item_value.saturating_mul(*quantity as i32))
                    } else {
                        acc
                    }
                });

        let mut my_items_to_remove = Vec::new();

        for (slot_id, amount) in my_offer {
            let item = my_inventory.get(*slot_id).ok_or("Failed to get item")?;
            let item_id = item.persistence_item_id();

            if item_id == COINS {
                continue;
            }

            if !self.sell_prices.contains_key(&item_id) {
                my_items_to_remove.push((*slot_id, *amount));
            }
        }

        let mut their_items_to_remove = Vec::new();

        for (slot_id, amount) in their_offer {
            let item = their_inventory.get(*slot_id).ok_or("Failed to get item")?;

            let item_id = item.persistence_item_id();

            if item_id == COINS {
                continue;
            }

            if !self.buy_prices.contains_key(&item_id) {
                their_items_to_remove.push((*slot_id, *amount));
            }
        }

        drop(inventories);

        if !self.is_player_notified {
            self.send_price_info(&trade.parties[their_offer_index])?;

            self.is_player_notified = true;
        }

        if their_offered_items_value == 0 && my_offered_items_value == 0 {
            return Ok(());
        }

        if !my_items_to_remove.is_empty() {
            for (item, quantity) in my_items_to_remove {
                self.client.perform_trade_action(TradeAction::RemoveItem {
                    item,
                    quantity,
                    ours: true,
                });
            }

            return Ok(());
        }

        if !their_items_to_remove.is_empty() {
            for (item, quantity) in their_items_to_remove {
                self.client.perform_trade_action(TradeAction::RemoveItem {
                    item,
                    quantity,
                    ours: false,
                });
            }

            return Ok(());
        }

        if my_offered_items_value > their_total_coin_amount {
            self.client.send_command(
                "tell".to_string(),
                vec![
                    self.find_name(&trade.parties[their_offer_index])
                        .ok_or("Failed to get uid")?
                        .to_string(),
                    format!("I need {my_offered_items_value} coins or trade value from you."),
                ],
            );

            return Ok(());
        }

        let difference = their_offered_items_value - my_offered_items_value;

        // If the trade is balanced
        if difference == 0 {
            // Accept
            self.client
                .perform_trade_action(TradeAction::Accept(trade.phase));
        // If they are offering more
        } else if difference.is_positive() {
            // If they are offering coins
            if their_offered_coin_amount > 0 {
                // Remove their coins to balance
                self.client.perform_trade_action(TradeAction::RemoveItem {
                    item: their_coins,
                    quantity: difference as u32,
                    ours: false,
                });
            // If they are not offering coins
            } else {
                // Add my coins to balanace
                self.client.perform_trade_action(TradeAction::AddItem {
                    item: my_coins,
                    quantity: difference as u32,
                    ours: true,
                });
            }
        // If I am offering more
        } else if difference.is_negative() {
            // If I am offering coins
            if my_offered_coin_amount > 0 {
                // Remove my coins to balance
                self.client.perform_trade_action(TradeAction::RemoveItem {
                    item: my_coins,
                    quantity: difference.unsigned_abs(),
                    ours: true,
                });
            // If I am not offering coins
            } else {
                // Add their coins to balance
                self.client.perform_trade_action(TradeAction::AddItem {
                    item: their_coins,
                    quantity: difference.unsigned_abs(),
                    ours: false,
                });
            }
        }

        Ok(())
    }

    fn handle_take(&mut self, trade: PendingTrade) -> Result<(), String> {
        let my_offer_index = trade
            .which_party(self.client.uid().ok_or("Failed to get uid")?)
            .ok_or("Failed to get offer index")?;
        let their_offer_index = if my_offer_index == 0 { 1 } else { 0 };
        let (my_offer, their_offer) = {
            (
                &trade.offers[my_offer_index],
                &trade.offers[their_offer_index],
            )
        };

        if my_offer.is_empty() && !their_offer.is_empty() {
            self.client
                .perform_trade_action(TradeAction::Accept(trade.phase));
        }

        Ok(())
    }

    fn send_price_info(&mut self, target: &Uid) -> Result<(), String> {
        let player_name = self
            .find_name(target)
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
            vec![player_name, format!("Sell prices: {:?}", self.sell_prices)],
        );

        Ok(())
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

    fn _find_uid<'a>(&'a self, name: &str) -> Option<&'a Uid> {
        self.client.player_list().iter().find_map(|(id, info)| {
            if info.player_alias == name {
                Some(id)
            } else {
                None
            }
        })
    }

    fn _find_uuid(&self, name: &str) -> Option<String> {
        self.client.player_list().iter().find_map(|(_, info)| {
            if info.player_alias == name {
                Some(info.uuid.to_string())
            } else {
                None
            }
        })
    }

    fn handle_position_and_orientation(&mut self) -> Result<(), String> {
        match self.client.position() {
            Some(current_position) => {
                if current_position == self.position.into() {
                    return Ok(());
                }
            }
            None => return Ok(()),
        }

        let entity = self.client.entity();
        let ecs = self.client.state_mut().ecs();
        let mut position_state = ecs.write_storage::<Pos>();
        let mut orientation_state = ecs.write_storage::<Ori>();
        let orientation = match self.orientation.to_lowercase().as_str() {
            "west" => Ori::default()
                .uprighted()
                .rotated(Quaternion::rotation_z(90.0.to_radians())),
            "south" => Ori::default()
                .uprighted()
                .rotated(Quaternion::rotation_z(180.0.to_radians())),
            "east" => Ori::default()
                .uprighted()
                .rotated(Quaternion::rotation_z(270.0.to_radians())),
            "north" => Ori::default(),
            _ => {
                return Err("Orientation must north, east, south or west".to_string());
            }
        };

        orientation_state
            .insert(entity, orientation)
            .map_err(|error| error.to_string())?;
        position_state
            .insert(entity, Pos(self.position.into()))
            .map_err(|error| error.to_string())?;

        Ok(())
    }
}

enum TradeMode {
    Take,
    Trade,
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
