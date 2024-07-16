/**
A bot that buys, sells and trades with players.

This bot is designed to run on the official Veloren server. It will connect to the server, select
a character, and then attempt to buy, sell, and trade with other players. The bot will also
announce its presence and respond to chat messages.

See [main.rs] for an example of how to run this bot.
**/
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::runtime::Runtime;
use vek::Quaternion;
use veloren_client::{addr::ConnectionArgs, Client, Event as VelorenEvent, SiteInfoRich, WorldExt};
use veloren_client_i18n::Localization;
use veloren_common::{
    clock::Clock,
    comp::{
        invite::InviteKind,
        item::{ItemDef, ItemDefinitionIdOwned, ItemI18n},
        slot::InvSlotId,
        ChatType, ControllerInputs, ItemKey, Ori, Pos,
    },
    outcome::Outcome,
    time::DayPeriod,
    trade::{PendingTrade, TradeAction, TradeResult},
    uid::Uid,
    uuid::Uuid,
    DamageSource, ViewDistances,
};
use veloren_common_net::sync::WorldSyncExt;

const COINS: &str = "common.items.utility.coins";

/// An active connection to the Veloren server that will attempt to run every time the `tick`
/// function is called.
///
/// See the [module-level documentation](index.html) for more information.
pub struct Bot {
    username: String,
    position: [f32; 3],
    orientation: f32,
    admins: Vec<String>,
    announcement: Option<String>,

    client: Client,
    clock: Clock,

    buy_prices: HashMap<String, u32>,
    sell_prices: HashMap<String, u32>,
    trade_mode: TradeMode,

    previous_offer: Option<(HashMap<String, u32>, HashMap<String, u32>)>,
    last_trade_action: Instant,
    last_announcement: Instant,
    last_ouch: Instant,
    sort_count: u8,
}

impl Bot {
    /// Connect to the official veloren server, select the specified character
    /// and return a Bot instance ready to run.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        username: String,
        password: &str,
        character: &str,
        admins: Vec<String>,
        buy_prices: HashMap<String, u32>,
        sell_prices: HashMap<String, u32>,
        position: [f32; 3],
        orientation: f32,
        announcement: Option<String>,
    ) -> Result<Self, String> {
        log::info!("Connecting to veloren");

        let mut client = connect_to_veloren(&username, password)?;
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
            .ok_or_else(|| format!("No character named {character}"))?
            .character
            .id
            .ok_or("Failed to get character ID")?;

        // This loop waits and retries requesting the character in the case that the character has
        // logged out to recently.
        while client.position().is_none() {
            client.request_character(
                character_id,
                ViewDistances {
                    terrain: 4,
                    entity: 4,
                },
            );

            client
                .tick(ControllerInputs::default(), clock.dt())
                .map_err(|error| format!("{error:?}"))?;
            clock.tick();
        }

        let now = Instant::now();

        Ok(Bot {
            username,
            position,
            orientation,
            admins,
            client,
            clock,
            buy_prices,
            sell_prices,
            trade_mode: TradeMode::Trade,
            previous_offer: None,
            last_trade_action: now,
            last_announcement: now,
            last_ouch: now,
            sort_count: 0,
            announcement,
        })
    }

    /// Run the bot for a single tick. This should be called in a loop.
    ///
    /// There are three timers in this function:
    /// - The [Clock] runs the Veloren client. At **30 ticks per second** this timer is faster than
    ///   the others so the bot can respond to events quickly.
    /// - `last_trade_action` times the bot's behavior to compensate for latency, every **300ms**.
    /// - `last_announcement` times the bot's announcements to **45 minutes** and is checked while
    ///   processing trade actions.
    ///
    /// This function should be modified with care. In addition to being the bot's main loop, it
    /// also accepts incoming trade invites, which has a potential for error if the bot accepts an
    /// invite while in the wrong trade mode.
    pub fn tick(&mut self) -> Result<(), String> {
        let veloren_events = self
            .client
            .tick(ControllerInputs::default(), self.clock.dt())
            .map_err(|error| format!("{error:?}"))?;

        for event in veloren_events {
            self.handle_veloren_event(event)?;
        }

        if self.last_trade_action.elapsed() > Duration::from_millis(300) {
            self.client.respawn();
            self.handle_position_and_orientation()?;
            self.handle_lantern();

            if let Some((_, trade, _)) = self.client.pending_trade() {
                match self.trade_mode {
                    TradeMode::AdminAccess => {
                        if !trade.is_empty_trade() {
                            self.client
                                .perform_trade_action(TradeAction::Accept(trade.phase));
                        }
                    }
                    TradeMode::Trade => self.handle_trade(trade.clone())?,
                }
            } else if self.client.pending_invites().is_empty() {
                match self.trade_mode {
                    TradeMode::AdminAccess => {
                        self.trade_mode = TradeMode::Trade;
                    }
                    TradeMode::Trade => {
                        self.client.accept_invite();
                    }
                }
            }

            if self.sort_count > 0 {
                self.client.sort_inventory();

                self.sort_count -= 1;

                if self.sort_count == 0 {
                    log::info!("Sorted inventory, finished")
                } else {
                    log::info!("Sorted inventory, {} more times to go", self.sort_count);
                }
            }

            if self.last_announcement.elapsed() > Duration::from_mins(45) {
                self.handle_announcement()?;

                self.last_announcement = Instant::now();
            }

            self.last_trade_action = Instant::now();
        }

        self.clock.tick();

        Ok(())
    }

    /// Consume and manage a client-side Veloren event.
    fn handle_veloren_event(&mut self, event: VelorenEvent) -> Result<(), String> {
        match event {
            VelorenEvent::Chat(message) => {
                let sender = if let ChatType::Tell(uid, _) = message.chat_type {
                    uid
                } else {
                    return Ok(());
                };
                let content = message.content().as_plain().unwrap_or_default();
                let mut split_content = content.split(' ');
                let command = split_content.next().unwrap_or_default();
                let price_correction_message = "Use the format 'price [search_term]'";
                let correction_message = match command {
                    "admin_access" => {
                        if self.is_user_admin(&sender)? && !self.client.is_trading() {
                            log::info!("Providing admin access");

                            self.previous_offer = None;
                            self.trade_mode = TradeMode::AdminAccess;

                            self.client.send_invite(sender, InviteKind::Trade);

                            None
                        } else {
                            Some(price_correction_message)
                        }
                    }
                    "announce" => {
                        if self.is_user_admin(&sender)? {
                            self.handle_announcement()?;

                            self.last_announcement = Instant::now();

                            None
                        } else {
                            Some(price_correction_message)
                        }
                    }
                    "ori" => {
                        if self.is_user_admin(&sender)? {
                            if let Some(orientation) = split_content.next() {
                                self.orientation = orientation
                                    .parse::<f32>()
                                    .map_err(|error| error.to_string())?;

                                None
                            } else {
                                Some("Use the format 'orientation [0-360]'")
                            }
                        } else {
                            Some(price_correction_message)
                        }
                    }
                    "price" => {
                        for item_name in split_content {
                            self.send_price_info(&sender, &item_name)?;
                        }

                        None
                    }
                    "pos" => {
                        if self.is_user_admin(&sender)? {
                            if let (Some(x), Some(y), Some(z)) = (
                                split_content.next(),
                                split_content.next(),
                                split_content.next(),
                            ) {
                                let position = [
                                    x.parse::<f32>().map_err(|error| error.to_string())?,
                                    y.parse::<f32>().map_err(|error| error.to_string())?,
                                    z.parse::<f32>().map_err(|error| error.to_string())?,
                                ];

                                self.position = position;

                                None
                            } else {
                                Some("Use the format 'position [x] [y] [z]'.")
                            }
                        } else {
                            Some(price_correction_message)
                        }
                    }
                    "sort" => {
                        if self.is_user_admin(&sender)? {
                            if let Some(sort_count) = split_content.next() {
                                let sort_count = sort_count
                                    .parse::<u8>()
                                    .map_err(|error| error.to_string())?;

                                log::info!("Sorting inventory {sort_count} times");

                                self.sort_count = sort_count;
                            } else {
                                self.client.sort_inventory();

                                log::info!("Sorting inventory once");
                            }

                            None
                        } else {
                            Some(price_correction_message)
                        }
                    }
                    _ => Some(price_correction_message),
                };

                if let Some(message) = correction_message {
                    let player_name = self
                        .find_name(&sender)
                        .ok_or("Failed to find player name")?
                        .to_string();

                    self.client.send_command(
                        "tell".to_string(),
                        vec![player_name.clone(), message.to_string()],
                    );
                }
            }
            VelorenEvent::Outcome(Outcome::ProjectileHit {
                target: Some(target),
                ..
            }) => {
                if let Some(uid) = self.client.uid() {
                    if uid == target && self.last_ouch.elapsed() > Duration::from_secs(2) {
                        self.client
                            .send_command("say".to_string(), vec!["Ouch!".to_string()]);

                        self.last_ouch = Instant::now();
                    }
                }
            }
            VelorenEvent::Outcome(Outcome::HealthChange { info, .. }) => {
                if let Some(DamageSource::Buff(_)) = info.cause {
                    return Ok(());
                }

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
            VelorenEvent::TradeComplete { result, trade } => {
                let my_party = trade
                    .which_party(self.client.uid().ok_or("Failed to find uid")?)
                    .ok_or("Failed to find trade party")?;
                let their_party = if my_party == 0 { 1 } else { 0 };
                let their_uid = trade.parties[their_party];
                let their_name = self.find_name(&their_uid).ok_or("Failed to find name")?;

                match result {
                    TradeResult::Completed => {
                        if let Some(offer) = &self.previous_offer {
                            log::info!("Trade with {their_name}: {offer:?}",);
                        }

                        self.client.send_command(
                            "say".to_string(),
                            vec!["Thank you for trading with me!".to_string()],
                        );
                    }
                    TradeResult::Declined => log::info!("Trade with {their_name} declined"),
                    TradeResult::NotEnoughSpace => {
                        log::info!("Trade with {their_name} failed: not enough space")
                    }
                }

                if let TradeMode::AdminAccess = self.trade_mode {
                    log::info!("End of admin access");

                    self.trade_mode = TradeMode::Trade;
                }
            }
            _ => (),
        }

        Ok(())
    }

    /// Make the bot's trading and help accouncements
    ///
    /// Currently, this can make two announcements: one in /region with basic usage instructions
    /// is always made. If an announcement was provided when the bot was created, it will make it
    /// in /world.
    fn handle_announcement(&mut self) -> Result<(), String> {
        log::info!("Making an announcement");

        self.client.send_command(
            "region".to_string(),
            vec![format!(
                "I'm a bot. You can trade with me or check prices: '/tell {} price [search_term]'.",
                self.username
            )],
        );

        if let Some(announcement) = &self.announcement {
            let location = self
                .client
                .sites()
                .into_iter()
                .find_map(|(_, SiteInfoRich { site, .. })| {
                    let x_difference = self.position[0] - site.wpos[0] as f32;
                    let y_difference = self.position[1] - site.wpos[1] as f32;

                    if x_difference.abs() < 100.0 && y_difference.abs() < 100.0 {
                        site.name.clone()
                    } else {
                        None
                    }
                })
                .unwrap_or(format!("{:?}", self.position));
            let interpolated_announcement = announcement.replace("{location}", &location);

            self.client
                .send_command("world".to_string(), vec![interpolated_announcement]);
        }

        Ok(())
    }

    /// Use the lantern at night and put it away during the day.
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

    /// Manage an active trade.
    ///
    /// This is a rather complex function that should be modified with care. The bot uses its buy
    /// and sell prices to determine an item's value and determines the total value of each side of
    /// the trade. Coins are hard-coded to have a value of 1 each.
    ///
    /// The bot's trading logic is as follows:
    ///
    /// 1. If the trade is empty, do nothing.
    /// 2. If my offer includes items I am not selling, remove those items unless they are coins.
    /// 3. If their offer includes items I am not buying, remove those items unless they are coins.
    /// 4. If the trade is balanced, accept it.
    /// 5. If the total value of their offer is greater than the total value of my offer:
    ///     1. If they are offering coins, remove them to balance.
    ///     2. If they are not offering coins, add mine to balance.
    /// 6. If the total value of my offer is greater than the total value of their offer:
    ///     1. If I am offering coins, remove them to balance.
    ///     2. If I am not offering coins, add theirs to balance.
    ///
    /// See the inline comments for more details.
    #[allow(clippy::comparison_chain)]
    fn handle_trade(&mut self, trade: PendingTrade) -> Result<(), String> {
        if trade.is_empty_trade() {
            return Ok(());
        }

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

        let mut my_item_to_remove = None;

        for (slot_id, amount) in my_offer {
            let item = my_inventory.get(*slot_id).ok_or("Failed to get item")?;
            let item_id = item.persistence_item_id();

            if item_id == COINS {
                continue;
            }

            if !self.sell_prices.contains_key(&item_id) {
                my_item_to_remove = Some((slot_id, amount));
            }
        }

        let mut their_item_to_remove = None;

        for (slot_id, amount) in their_offer {
            let item = their_inventory.get(*slot_id).ok_or("Failed to get item")?;
            let item_id = item.persistence_item_id();

            if item_id == COINS {
                continue;
            }

            if !self.buy_prices.contains_key(&item_id) {
                their_item_to_remove = Some((slot_id, amount));
            }
        }

        let item_offers = {
            let mut their_items = HashMap::with_capacity(their_offer.len());

            for (slot_id, quantity) in their_offer {
                if let Some(item) = their_inventory.get(*slot_id) {
                    their_items.insert(item.persistence_item_id(), *quantity);
                }
            }

            let mut my_items = HashMap::with_capacity(my_offer.len());

            for (slot_id, quantity) in my_offer {
                if let Some(item) = my_inventory.get(*slot_id) {
                    my_items.insert(item.persistence_item_id(), *quantity);
                }
            }

            (my_items, their_items)
        };

        drop(inventories);

        // Before running any actual trade logic, remove items that are not for sale or not being
        // purchased. End this trade action if an item was removed.

        if let Some((slot_id, quantity)) = my_item_to_remove {
            self.client.perform_trade_action(TradeAction::RemoveItem {
                item: *slot_id,
                quantity: *quantity,
                ours: true,
            });

            return Ok(());
        }

        if let Some((slot_id, quantity)) = their_item_to_remove {
            self.client.perform_trade_action(TradeAction::RemoveItem {
                item: *slot_id,
                quantity: *quantity,
                ours: false,
            });

            return Ok(());
        }

        let difference = their_offered_items_value - my_offered_items_value;

        // The if/else statements below implement the bot's main feature: buying, selling and
        // trading items according to the values set in the configuration file. In the case that
        // either the bot or the other player does not have any coins, the bot will not send an
        // error and the trade will remain unbalanced. In the case that we try to add more coins
        // than are available, the server will just add all of the available coins and the trade
        // will remain unbalanced.

        // If the trade is balanced
        if difference == 0 {
            self.previous_offer = Some(item_offers);

            // Accept
            self.client
                .perform_trade_action(TradeAction::Accept(trade.phase));
        // If they are offering more
        } else if difference > 0 {
            // If they are offering coins
            if their_offered_coin_amount > 0 {
                if let Some(their_coins) = get_their_coins {
                    // Remove their coins to balance
                    self.client.perform_trade_action(TradeAction::RemoveItem {
                        item: their_coins,
                        quantity: difference as u32,
                        ours: false,
                    });
                }
            // If they are not offering coins
            } else if let Some(my_coins) = get_my_coins {
                // Add my coins to balanace
                self.client.perform_trade_action(TradeAction::AddItem {
                    item: my_coins,
                    quantity: difference as u32,
                    ours: true,
                });
            }
        // If I am offering more
        } else if difference < 0 {
            // If I am offering coins
            if my_offered_coin_amount > 0 {
                if let Some(my_coins) = get_my_coins {
                    // Remove my coins to balance
                    self.client.perform_trade_action(TradeAction::RemoveItem {
                        item: my_coins,
                        quantity: difference.unsigned_abs(),
                        ours: true,
                    });
                }
            // If I am not offering coins
            } else if let Some(their_coins) = get_their_coins {
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

    /// Attempts to find an item based on a search term and sends the price info to the target
    /// player.
    fn send_price_info(&mut self, target: &Uid, search_term: &str) -> Result<(), String> {
        let player_name = self
            .find_name(target)
            .ok_or("Failed to find player name")?
            .to_string();
        let mut found = false;

        for (item_id, price) in &self.buy_prices {
            if item_id.contains(search_term) {
                let short_id = item_id.splitn(3, '.').last().unwrap_or_default();

                log::info!("Sending price info on {short_id} to {player_name}");

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
            if item_id.contains(search_term) {
                let short_id = item_id.splitn(3, '.').last().unwrap_or_default();

                log::info!("Sending price info on {short_id} to {player_name}");

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
            log::info!("Found no price for \"{search_term}\" for {player_name}");

            self.client.send_command(
                "tell".to_string(),
                vec![player_name, format!("I don't have a price for that item.")],
            );
        }

        Ok(())
    }

    /// Determines if the Uid belonds to an admin.
    fn is_user_admin(&self, uid: &Uid) -> Result<bool, String> {
        let sender_uuid = self
            .find_uuid(uid)
            .ok_or("Failed to find uuid")?
            .to_string();
        let sender_name = self.find_name(uid).ok_or("Failed to find name")?;

        Ok(self.admins.contains(sender_name) || self.admins.contains(&sender_uuid))
    }

    /// Moves the character to the configured position and orientation.
    fn handle_position_and_orientation(&mut self) -> Result<(), String> {
        if let Some(current_position) = self.client.current::<Pos>() {
            let target_position = Pos(self.position.into());

            if current_position != target_position {
                let entity = self.client.entity();
                let ecs = self.client.state_mut().ecs();
                let mut position_state = ecs.write_storage::<Pos>();

                position_state
                    .insert(entity, target_position)
                    .map_err(|error| error.to_string())?;
            }
        }

        if let Some(current_orientation) = self.client.current::<Ori>() {
            let target_orientation = Ori::default()
                .uprighted()
                .rotated(Quaternion::rotation_z(self.orientation.to_radians()));

            if current_orientation != target_orientation {
                let entity = self.client.entity();
                let ecs = self.client.state_mut().ecs();
                let mut orientation_state = ecs.write_storage::<Ori>();

                orientation_state
                    .insert(entity, target_orientation)
                    .map_err(|error| error.to_string())?;
            }
        }

        Ok(())
    }

    /// Finds the name of a player by their Uid.
    fn find_name<'a>(&'a self, uid: &Uid) -> Option<&'a String> {
        self.client.player_list().iter().find_map(|(id, info)| {
            if id == uid {
                return Some(&info.player_alias);
            }

            None
        })
    }

    /// Finds the Uuid of a player by their Uid.
    fn find_uuid(&self, target: &Uid) -> Option<Uuid> {
        self.client.player_list().iter().find_map(|(uid, info)| {
            if uid == target {
                Some(info.uuid)
            } else {
                None
            }
        })
    }

    /// Finds the Uid of a player by their name.
    fn _find_uid<'a>(&'a self, name: &str) -> Option<&'a Uid> {
        self.client.player_list().iter().find_map(|(id, info)| {
            if info.player_alias == name {
                Some(id)
            } else {
                None
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TradeMode {
    AdminAccess,
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
