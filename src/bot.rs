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
    trade::{PendingTrade, TradeAction, TradeResult},
    uid::Uid,
    ViewDistances,
};
use veloren_common_net::sync::WorldSyncExt;

const COINS: &str = "common.items.utility.coins";

/// A Bot instance represents an active connection to the server and it will
/// attempt to run every time the `tick` function is called.
pub struct Bot {
    position: [f32; 3],
    orientation: String,

    client: Client,
    clock: Clock,

    buy_prices: HashMap<String, u32>,
    sell_prices: HashMap<String, u32>,
    trade_mode: TradeMode,

    last_action: Instant,
    last_announcement: Instant,
    last_ouch: Instant,
    last_cost_difference: i32,
}

impl Bot {
    /// Connect to the official veloren server, select the specified character
    /// return a Bot instance ready to run.
    pub fn new(
        username: &str,
        password: &str,
        character: &str,
        buy_prices: HashMap<String, u32>,
        sell_prices: HashMap<String, u32>,
        position: [f32; 3],
        orientation: String,
    ) -> Result<Self, String> {
        log::info!("Connecting to veloren");

        let mut client = connect_to_veloren(username, password)?;
        let mut clock = Clock::new(Duration::from_secs_f64(1.0 / 30.0));

        log::info!("Selecting a character");

        client.load_character_list();

        while client.character_list().loading {
            client
                .tick(ControllerInputs::default(), clock.dt())
                .map_err(|error| format!("{error:?}"))?;
            clock.tick();
        }

        let character_id = client
            .character_list()
            .characters
            .iter()
            .find(|character_item| character_item.character.alias == character)
            .expect(&format!("No character named {character}"))
            .character
            .id
            .expect("Failed to get character ID");

        client.request_character(
            character_id,
            ViewDistances {
                terrain: 4,
                entity: 4,
            },
        );

        let now = Instant::now();

        Ok(Bot {
            position,
            orientation,
            client,
            clock,
            buy_prices,
            sell_prices,
            trade_mode: TradeMode::Trade,
            last_action: now,
            last_announcement: now,
            last_ouch: now,
            last_cost_difference: 0,
        })
    }

    // Run the bot for a single tick. This should be called in a loop.
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
                self.client.accept_invite();
            }

            self.last_action = Instant::now();

            if self.last_announcement.elapsed() > Duration::from_secs(1200) {
                self.client.send_command(
                    "region".to_string(),
                    vec!["I'm a bot. You can trade with me or use /say or /tell to check prices: 'price [item_name]'".to_string()],
                );

                self.last_announcement = Instant::now();
            }
        }

        self.clock.tick();

        Ok(())
    }

    // Consume and manage a client-side Veloren event.
    fn handle_veloren_event(&mut self, event: VelorenEvent) -> Result<(), String> {
        match event {
            VelorenEvent::Chat(message) => {
                let sender = match message.chat_type {
                    ChatType::Tell(uid, _) => uid,
                    ChatType::Say(uid) => uid,
                    _ => return Ok(()),
                };
                let content = message.content().as_plain().unwrap_or_default();
                let (command, item_name) = content.split_once(' ').unwrap_or((content, ""));
                let item_name = item_name.to_lowercase();

                match command {
                    "price" => {
                        self.send_price_info(&sender, &item_name)?;
                    }
                    "take" => {
                        if !self.client.is_trading() {
                            self.trade_mode = TradeMode::Take;
                            self.client.send_invite(sender, InviteKind::Trade);
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
            VelorenEvent::Outcome(Outcome::HealthChange { info, .. }) => {
                if let Some(uid) = self.client.uid() {
                    if uid == info.target
                        && info.amount.is_sign_negative()
                        && self.last_ouch.elapsed() > Duration::from_secs(1)
                    {
                        self.client
                            .send_command("say".to_string(), vec!["That hurt!".to_string()]);

                        self.last_ouch = Instant::now();
                    }
                }
            }
            VelorenEvent::TradeComplete { result, .. } => {
                log::info!("Completed trade: {result:?}");

                if let TradeMode::Take = self.trade_mode {
                    self.trade_mode = TradeMode::Trade;
                }

                if let TradeResult::Completed = result {
                    self.client.send_command(
                        "say".to_string(),
                        vec!["Thank you for trading with me!".to_string()],
                    );
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

        if trade.is_empty_trade() {
            return Ok(());
        }

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
        let my_inventory = inventories
            .get(self.client.entity())
            .ok_or("Failed to find inventory")?;
        let get_my_coins = my_inventory
            .get_slot_of_item_by_def_id(&ItemDefinitionIdOwned::Simple(COINS.to_string()));
        let their_inventory = inventories.get(them).ok_or("Failed to find inventory")?;
        let get_their_coins = their_inventory
            .get_slot_of_item_by_def_id(&ItemDefinitionIdOwned::Simple(COINS.to_string()));
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

        let (my_coins, their_coins) =
            if let (Some(mine), Some(theirs)) = (get_my_coins, get_their_coins) {
                (mine, theirs)
            } else {
                return Ok(());
            };

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

        if difference != self.last_cost_difference {
            let their_name = self
                .find_name(&trade.parties[their_offer_index])
                .ok_or("Failed to find player name")?
                .to_string();

            log::debug!("Sending cost to {their_name}");

            self.client.send_command(
                "tell".to_string(),
                vec![their_name, format!("My offer: {my_offered_items_value}. Your offer: {their_offered_items_value}.")],
            );
        }

        self.last_cost_difference = difference;

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

    fn send_price_info(&mut self, target: &Uid, item_name: &str) -> Result<(), String> {
        let player_name = self
            .find_name(target)
            .ok_or("Failed to find player name")?
            .to_string();
        let mut found = false;

        for (item_id, price) in &self.buy_prices {
            if item_id.contains(item_name) {
                let short_id = item_id.splitn(3, '.').last().unwrap_or_default();

                log::debug!("Sending price info on {short_id} to {player_name}");

                self.client.send_command(
                    "tell".to_string(),
                    vec![
                        player_name.clone(),
                        format!("Buying {short_id} for {price} coins."),
                    ],
                );

                found = true;
            }
        }

        for (item_id, price) in &self.sell_prices {
            if item_id.contains(item_name) {
                let short_id = item_id.splitn(3, '.').last().unwrap_or_default();

                log::debug!("Sending price info on {short_id} to {player_name}");

                self.client.send_command(
                    "tell".to_string(),
                    vec![
                        player_name.clone(),
                        format!("Selling {short_id} for {price} coins."),
                    ],
                );

                found = true;
            }
        }

        if !found {
            log::debug!("Found no price for \"{item_name}\" for {player_name}");

            self.client.send_command(
                "tell".to_string(),
                vec![
                    player_name.clone(),
                    format!("I don't have a price for that item."),
                ],
            );
        }

        Ok(())
    }

    fn find_name<'a>(&'a self, uid: &Uid) -> Option<&'a String> {
        self.client.player_list().iter().find_map(|(id, info)| {
            if id == uid {
                return Some(&info.player_alias);
            }

            None
        })
    }

    fn handle_position_and_orientation(&mut self) -> Result<(), String> {
        let current_position = self.client.current::<Pos>();

        if let Some(current_position) = current_position {
            if current_position.0 == self.position.into() {
                return Ok(());
            }
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
