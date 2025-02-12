/// A bot that buys, sells and trades with players.
///
/// See [main.rs] for an example of how to run this bot.
use std::{
    borrow::Cow,
    sync::Arc,
    time::{Duration, Instant},
};

use hashbrown::HashMap;
use log::{debug, info};
use tokio::runtime::Runtime;
use vek::{Quaternion, Vec3};
use veloren_client::{
    addr::ConnectionArgs, Client, ClientType, Event as VelorenEvent, SiteInfoRich, WorldExt,
};
use veloren_client_i18n::LocalizationHandle;
use veloren_common::{
    clock::Clock,
    comp::{
        invite::InviteKind,
        item::{ItemDefinitionId, ItemDesc, ItemI18n, MaterialStatManifest},
        tool::AbilityMap,
        ChatType, ControllerInputs, Item, Ori, Pos,
    },
    time::DayPeriod,
    trade::{PendingTrade, TradeAction, TradeResult},
    uid::Uid,
    uuid::Uuid,
    ViewDistances,
};
use veloren_common_net::sync::WorldSyncExt;

use crate::config::PriceList;

const COINS: ItemDefinitionId =
    ItemDefinitionId::Simple(Cow::Borrowed("common.items.utility.coins"));
const CLIENT_TPS: Duration = Duration::from_millis(33);
const TRADE_ACTION_DELAY: Duration = Duration::from_millis(300);
const ACCOUNCEMENT_DELAY: Duration = Duration::from_mins(45);

/// An active connection to the Veloren server that will attempt to run every time the `tick`
/// function is called.
pub struct Bot {
    position: Pos,
    orientation: Ori,
    admins: Vec<String>,
    announcement: Option<String>,

    client: Client,
    clock: Clock,
    ability_map: AbilityMap,
    material_manifest: MaterialStatManifest,
    item_i18n: ItemI18n,
    localization: LocalizationHandle,

    buy_prices: PriceList,
    sell_prices: PriceList,
    trade_mode: TradeMode,

    previous_trade: Option<PendingTrade>,
    previous_trade_receipt: Option<Reciept>,
    last_trade_action: Instant,
    last_announcement: Instant,
    sort_count: u8,
}

impl Bot {
    /// Connect to the official veloren server, select the specified character
    /// and return a Bot instance ready to run.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        game_server: String,
        auth_server: &str,
        username: String,
        password: &str,
        character: &str,
        admins: Vec<String>,
        buy_prices: PriceList,
        sell_prices: PriceList,
        position: Option<[f32; 3]>,
        orientation: Option<f32>,
        announcement: Option<String>,
    ) -> Result<Self, String> {
        info!("Connecting to veloren");

        let mut client = connect_to_veloren(game_server, auth_server, &username, password)?;
        let mut clock = Clock::new(CLIENT_TPS);

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

        info!("Selecting a character");

        // This loop waits and retries requesting the character in the case that the character has
        // logged out too recently.
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

        client.request_player_physics(false);

        let position = if let Some(coords) = position {
            Pos(coords.into())
        } else {
            client.position().map(Pos).ok_or("Failed to get position")?
        };
        let orientation = if let Some(orientation) = orientation {
            Ori::new(Quaternion::rotation_z(orientation.to_radians()))
        } else {
            client.current::<Ori>().ok_or("Failed to get orientation")?
        };
        let now = Instant::now();

        Ok(Bot {
            position,
            orientation,
            admins,
            client,
            clock,
            ability_map: AbilityMap::load().read().clone(),
            material_manifest: MaterialStatManifest::load().read().clone(),
            item_i18n: ItemI18n::new_expect(),
            localization: LocalizationHandle::load_expect("en"),
            buy_prices,
            sell_prices,
            trade_mode: TradeMode::Trade,
            previous_trade: None,
            previous_trade_receipt: None,
            last_trade_action: now,
            last_announcement: now,
            sort_count: 0,
            announcement,
        })
    }

    /// Run the bot for a single tick. This should be called in a loop. Returns `true` if the loop
    /// should continue running.
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
    pub fn tick(&mut self) -> Result<bool, String> {
        let veloren_events = self
            .client
            .tick(ControllerInputs::default(), self.clock.dt())
            .map_err(|error| format!("{error:?}"))?;

        for event in veloren_events {
            let should_continue = self.handle_veloren_event(event)?;

            if !should_continue {
                return Ok(false);
            }
        }

        if self.last_trade_action.elapsed() > TRADE_ACTION_DELAY {
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
                        // This should never happen, but in case the server fails to send a trade
                        // invite, the bot will switch to trade mode.
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
                    debug!("Sorted inventory, finished")
                } else {
                    debug!("Sorted inventory, {} more times to go", self.sort_count);
                }
            }

            if self.last_announcement.elapsed() > ACCOUNCEMENT_DELAY {
                self.handle_announcement()?;

                self.last_announcement = Instant::now();
            }

            self.last_trade_action = Instant::now();
        }

        self.clock.tick();

        Ok(true)
    }

    /// Consume and manage a client-side Veloren event. Returns a boolean indicating whether the
    /// bot should continue processing events.
    fn handle_veloren_event(&mut self, event: VelorenEvent) -> Result<bool, String> {
        match event {
            VelorenEvent::Chat(message) => {
                let sender = if let ChatType::Tell(uid, _) = message.chat_type {
                    uid
                } else {
                    return Ok(true);
                };
                let content = message.content().as_plain().unwrap_or_default();
                let mut split_content = content.split(' ');
                let command = split_content.next().unwrap_or_default();
                let price_correction_message = "Use the format 'price [search_term]'";
                let correction_message = match command {
                    "admin_access" => {
                        if self.is_user_admin(&sender)? {
                            info!("Providing admin access");

                            self.previous_trade = None;
                            self.trade_mode = TradeMode::AdminAccess;

                            if self.client.is_trading() {
                                self.client.perform_trade_action(TradeAction::Decline);
                            }

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
                    "location" => {
                        self.send_location_info(&sender)?;

                        None
                    }
                    "ori" => {
                        if self.is_user_admin(&sender)? {
                            if let Some(new_rotation) = split_content.next() {
                                let new_rotation = new_rotation
                                    .parse::<f32>()
                                    .map_err(|error| error.to_string())?;

                                self.orientation =
                                    Ori::new(Quaternion::rotation_z(new_rotation.to_radians()));

                                None
                            } else {
                                Some("Use the format 'ori [0-360]'")
                            }
                        } else {
                            Some(price_correction_message)
                        }
                    }
                    "price" => {
                        for item_name in split_content {
                            self.send_price_info(&sender, item_name)?;
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
                                self.position = Pos(Vec3::new(
                                    x.parse::<f32>().map_err(|error| error.to_string())?,
                                    y.parse::<f32>().map_err(|error| error.to_string())?,
                                    z.parse::<f32>().map_err(|error| error.to_string())?,
                                ));

                                None
                            } else {
                                Some("Use the format 'pos [x] [y] [z]'.")
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

                                debug!("Sorting inventory {sort_count} times");

                                self.sort_count = sort_count;
                            } else {
                                self.client.sort_inventory();

                                debug!("Sorting inventory once");
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
                        .find_player_alias(&sender)
                        .ok_or("Failed to find player name")?
                        .to_string();

                    self.client.send_command(
                        "tell".to_string(),
                        vec![player_name.clone(), message.to_string()],
                    );
                }
            }
            VelorenEvent::TradeComplete { result, trade } => {
                let my_party = trade
                    .which_party(self.client.uid().ok_or("Failed to find uid")?)
                    .ok_or("Failed to find trade party")?;
                let their_party = if my_party == 0 { 1 } else { 0 };
                let their_uid = trade.parties[their_party];
                let their_name = self
                    .find_player_alias(&their_uid)
                    .ok_or("Failed to find name")?
                    .clone();

                match result {
                    TradeResult::Completed => {
                        if let Some(reciept) = &self.previous_trade_receipt {
                            info!("Trade with {their_name}: {:?}", reciept);
                        }

                        self.client.send_command(
                            "say".to_string(),
                            vec!["Thank you for trading with me!".to_string()],
                        );
                    }
                    TradeResult::Declined => info!("Trade with {their_name} declined"),
                    TradeResult::NotEnoughSpace => {
                        info!("Trade with {their_name} failed: not enough space")
                    }
                }

                if let TradeMode::AdminAccess = self.trade_mode {
                    info!("End of admin access for {their_name}");

                    self.trade_mode = TradeMode::Trade;
                }
            }
            VelorenEvent::Disconnect => {
                return Ok(false);
            }
            _ => (),
        }

        Ok(true)
    }

    fn send_location_info(&mut self, target: &Uid) -> Result<(), String> {
        let player_name = self
            .find_player_alias(target)
            .ok_or("Failed to find player alias")?
            .to_string();
        let location = self
            .client
            .sites()
            .into_iter()
            .find_map(|(_, SiteInfoRich { site, .. })| {
                let x_difference = self.position.0[0] - site.wpos[0] as f32;
                let y_difference = self.position.0[1] - site.wpos[1] as f32;

                if x_difference.abs() < 100.0 && y_difference.abs() < 100.0 {
                    site.name.clone()
                } else {
                    None
                }
            })
            .unwrap_or(format!("{:?}", self.position));

        self.client.send_command(
            "tell".to_string(),
            vec![player_name, format!("I am at {location}.")],
        );

        Ok(())
    }

    /// Make the bot's trading and help accouncements
    ///
    /// Currently, this can make two announcements: one in /region with basic usage instructions
    /// is always made. If an announcement was provided when the bot was created, it will make it
    /// in /world.
    fn handle_announcement(&mut self) -> Result<(), String> {
        if let Some(announcement) = &self.announcement {
            debug!("Making an announcement");

            self.client
                .send_command("region".to_string(), vec![announcement.to_string()]);
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
    /// 1. If the trade is empty or hasn't changed, do nothing.
    /// 2. If my offer includes items I am not selling, remove those items unless they are coins.
    /// 3. If their offer includes items I am not buying, remove those items unless they are coins.
    /// 4. If the trade is balanced, accept it.
    /// 5. If the total value of their offer is greater than the total value of my offer:
    ///     1. If they are offering coins, remove them to balance.
    ///     2. If they are not offering coins, add mine to balance if I have enough.
    /// 6. If the total value of my offer is greater than the total value of their offer:
    ///     1. If I am offering coins, remove them to balance.
    ///     2. If I am not offering coins, add theirs to balance if they have enough.
    /// 7. If the trade is still unbalanced, tell them the value of the greater offer and the
    ///    other party's total coin amount.
    ///
    /// See the inline comments for more details.
    #[allow(clippy::comparison_chain)]
    fn handle_trade(&mut self, trade: PendingTrade) -> Result<(), String> {
        if trade.is_empty_trade() {
            return Ok(());
        }

        let phase = trade.phase;
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
        let their_uid = trade.parties[their_offer_index];
        let their_name = self
            .find_player_alias(&their_uid)
            .ok_or("Failed to find player name")?
            .clone();

        let inventories = self.client.inventories();
        let me = self.client.entity();
        let them = self
            .client
            .state()
            .ecs()
            .entity_from_uid(trade.parties[their_offer_index])
            .ok_or("Failed to find player".to_string())?;
        let (my_inventory, their_inventory) = (
            inventories.get(me).ok_or("Failed to find inventory")?,
            inventories.get(them).ok_or("Failed to find inventory")?,
        );

        let coins_owned = COINS.to_owned();
        let get_my_coins = my_inventory.get_slot_of_item_by_def_id(&coins_owned);
        let get_their_coins = their_inventory.get_slot_of_item_by_def_id(&coins_owned);
        let my_coin_amount = if let Some(coins) = get_my_coins {
            my_inventory.get(coins).unwrap().amount()
        } else {
            0
        };
        let their_coin_amount = if let Some(coins) = get_their_coins {
            their_inventory.get(coins).unwrap().amount()
        } else {
            0
        };
        let mut receipt = Reciept {
            my_items: HashMap::new(),
            their_items: HashMap::new(),
        };

        let (mut my_offered_coin_amount, mut their_offered_coin_amount) = (0, 0);
        let my_offered_items_value =
            my_offer
                .into_iter()
                .fold(0, |acc: i32, (slot_id, quantity)| {
                    if let Some(item) = my_inventory.get(*slot_id) {
                        let item_id = item.item_definition_id();

                        let item_value = if item_id == COINS {
                            my_offered_coin_amount = *quantity as i32;

                            1
                        } else {
                            self.sell_prices
                                .0
                                .get(&item_id)
                                .map(|int| *int as i32)
                                .unwrap_or_else(|| {
                                    self.buy_prices
                                        .0
                                        .get(&item_id)
                                        .map(|int| 0 - *int as i32)
                                        .unwrap_or(i32::MIN)
                                })
                        };

                        let item_name = self.get_item_name(item_id);

                        receipt.my_items.insert(item_name, *quantity);
                        acc.saturating_add(item_value.saturating_mul(*quantity as i32))
                    } else {
                        acc
                    }
                });
        let their_offered_items_value =
            their_offer
                .into_iter()
                .fold(0, |acc: i32, (slot_id, quantity)| {
                    if let Some(item) = their_inventory.get(*slot_id) {
                        let item_id = item.item_definition_id();

                        let item_value = if item_id == COINS {
                            their_offered_coin_amount = *quantity as i32;

                            1
                        } else {
                            self.buy_prices
                                .0
                                .get(&item_id)
                                .map(|int| *int as i32)
                                .unwrap_or_else(|| {
                                    self.sell_prices
                                        .0
                                        .get(&item_id)
                                        .map(|int| 0 - *int as i32)
                                        .unwrap_or(0)
                                })
                        };

                        let item_name = self.get_item_name(item_id);

                        receipt.their_items.insert(item_name, *quantity);
                        acc.saturating_add(item_value.saturating_mul(*quantity as i32))
                    } else {
                        acc
                    }
                });

        let mut my_item_to_remove = None;

        for (slot_id, amount) in my_offer {
            let item = my_inventory.get(*slot_id).ok_or("Failed to get item")?;
            let item_id = item.item_definition_id();

            if item_id == COINS {
                continue;
            }

            if !self.sell_prices.0.contains_key(&item_id) {
                info!("No sell price for {item_id:?}");

                my_item_to_remove = Some((*slot_id, *amount));
            }
        }

        let mut their_item_to_remove = None;

        for (slot_id, amount) in their_offer {
            let item = their_inventory.get(*slot_id).ok_or("Failed to get item")?;
            let item_id = item.item_definition_id();

            if item_id == COINS {
                continue;
            }

            if !self.buy_prices.0.contains_key(&item_id) {
                info!("No buy price for {item_id:?}");

                their_item_to_remove = Some((*slot_id, *amount));
            }
        }

        drop(inventories);

        // Before running any actual trade logic, remove items that are not for sale or not being
        // purchased. End this trade action if an item was removed.
        if let Some((slot_id, quantity)) = my_item_to_remove {
            self.client.perform_trade_action(TradeAction::RemoveItem {
                item: slot_id,
                quantity,
                ours: true,
            });

            return Ok(());
        }

        if let Some((slot_id, quantity)) = their_item_to_remove {
            self.client.perform_trade_action(TradeAction::RemoveItem {
                item: slot_id,
                quantity,
                ours: false,
            });

            return Ok(());
        }

        let difference = their_offered_items_value - my_offered_items_value;

        // If the trade hasn't changed, do nothing to avoid spamming the server.
        if let Some(previous) = &self.previous_trade {
            if previous == &trade {
                return Ok(());
            }
        }

        // Up until now there may have been an error, so we only update and check the previous
        // offer now. The trade action is infallible from here.
        self.previous_trade = Some(trade);

        debug!("Performing trade action with {their_name}");

        // The if/else statements below implement the bot's main feature: buying, selling and
        // trading items according to the values set in the configuration file. Coins are used to
        // balance the value of the trade. If there are not enough or no coins to balance the
        // trade, a message is sent to the user.

        // If the trade is balanced
        if difference == 0 {
            self.previous_trade_receipt = Some(receipt);

            // Accept
            self.client.perform_trade_action(TradeAction::Accept(phase));

            return Ok(());
        }

        // If they are offering more
        if difference > 0 {
            // If they are offering coins
            if their_offered_coin_amount > 0 {
                if let Some(their_coins) = get_their_coins {
                    // Remove their coins to balance
                    self.client.perform_trade_action(TradeAction::RemoveItem {
                        item: their_coins,
                        quantity: difference as u32,
                        ours: false,
                    });

                    return Ok(());
                }
            // If they are not offering coins
            } else if let Some(my_coins) = get_my_coins {
                // If I have enough coins
                if my_coin_amount >= difference as u32 {
                    // Add my coins to balanace
                    self.client.perform_trade_action(TradeAction::AddItem {
                        item: my_coins,
                        quantity: difference as u32,
                        ours: true,
                    });

                    return Ok(());
                }
            }

            self.client.send_command(
                "tell".to_string(),
                vec![
                    their_name,
                    format!(
                        "The value of your offer is {their_offered_items_value} coins. I only have {my_coin_amount} coins.",
                    ),
                ],
            );

            return Ok(());
        }

        // If I am offering more
        if difference < 0 {
            // If I am offering coins
            if my_offered_coin_amount > 0 {
                if let Some(my_coins) = get_my_coins {
                    // Remove my coins to balance
                    self.client.perform_trade_action(TradeAction::RemoveItem {
                        item: my_coins,
                        quantity: difference.unsigned_abs(),
                        ours: true,
                    });

                    return Ok(());
                }
            // If I am not offering coins
            } else if let Some(their_coins) = get_their_coins {
                // If they have enough coins
                if their_coin_amount >= difference.unsigned_abs() {
                    // Add their coins to balance
                    self.client.perform_trade_action(TradeAction::AddItem {
                        item: their_coins,
                        quantity: difference.unsigned_abs(),
                        ours: false,
                    });

                    return Ok(());
                }
            }

            self.client.send_command(
                "tell".to_string(),
                vec![
                    their_name,
                    format!(
                        "The value of my offer is {my_offered_items_value} coins. You only have {their_coin_amount} coins.",
                    ),
                ],
            );
        }

        Ok(())
    }

    /// Attempts to find an item based on a search term and sends the price info to the target
    /// player.
    ///
    /// The search is case-insensitive. It searches both the item name then, if the search term is
    /// not found, it searches the item's ID as written in the configuration file.
    fn send_price_info(&mut self, target: &Uid, search_term: &str) -> Result<(), String> {
        let original_search_term = search_term;
        let search_term = search_term.to_lowercase();
        let player_name = self
            .find_player_alias(target)
            .ok_or("Failed to find player name")?
            .to_string();
        let mut buying = Vec::new();
        let mut selling = Vec::new();

        for (item_id, price) in &self.buy_prices.0 {
            let item_name = self.get_item_name(item_id.as_ref());

            if item_name.to_lowercase().contains(&search_term) {
                buying.push((item_name, price));

                continue;
            }

            if let Some(item_id_string) = item_id.as_ref().itemdef_id() {
                if item_id_string.to_lowercase().contains(&search_term) {
                    buying.push((item_name, price));
                }
            }
        }

        let inventories = self.client.inventories();
        let my_inventory = inventories
            .get(self.client.entity())
            .ok_or("Failed to find inventory")?;

        for (item_id, price) in &self.sell_prices.0 {
            let item = Item::new_from_item_definition_id(
                item_id.as_ref(),
                &self.ability_map,
                &self.material_manifest,
            )
            .map_err(|error| error.to_string())?;
            let (item_name_i18n_id, _) = item.i18n(&self.item_i18n);
            let item_name = self.localization.read().get_content(&item_name_i18n_id);
            let item_inventory_slot = my_inventory.get_slot_of_item(&item);
            let stock = if let Some(slot_id) = item_inventory_slot {
                my_inventory.get(slot_id).unwrap().amount()
            } else {
                0
            };

            if item_name.to_lowercase().contains(&search_term) {
                selling.push((item_name, price, stock));

                continue;
            }

            if let Some(item_id_string) = item_id.as_ref().itemdef_id() {
                if item_id_string.to_lowercase().contains(&search_term) {
                    selling.push((item_name, price, stock));
                }
            }
        }

        drop(inventories);

        let total_found = buying.len() + selling.len();

        if total_found == 0 {
            debug!("Found no price for \"{original_search_term}\" for {player_name}");

            self.client.send_command(
                "tell".to_string(),
                vec![
                    player_name,
                    format!("I don't have a price for {original_search_term}."),
                ],
            );

            return Ok(());
        }

        if total_found > 10 {
            debug!(
                "Found {total_found} prices for \"{original_search_term}\" for {player_name}, not sending."
            );

            self.client.send_command(
                "tell".to_string(),
                vec![
                    player_name,
                    format!(
                        "I found {total_found} prices for {original_search_term}. Please be more specific."
                    ),
                ],
            );

            return Ok(());
        }

        debug!("Found {total_found} prices for \"{original_search_term}\" for {player_name}, sending prices.");

        for (item_name, price) in buying {
            self.client.send_command(
                "tell".to_string(),
                vec![
                    player_name.clone(),
                    format!("Buying {item_name} for {price} coins."),
                ],
            );
        }

        for (item_name, price, stock) in selling {
            self.client.send_command(
                "tell".to_string(),
                vec![
                    player_name.clone(),
                    format!("Selling {item_name} for {price} coins. I have {stock} in stock."),
                ],
            );
        }

        Ok(())
    }

    /// Determines if the Uid belongs to an admin.
    fn is_user_admin(&self, uid: &Uid) -> Result<bool, String> {
        let sender_name = self.find_player_alias(uid).ok_or("Failed to find name")?;

        if self.admins.contains(sender_name) {
            Ok(true)
        } else {
            let sender_uuid = self
                .find_uuid(uid)
                .ok_or("Failed to find uuid")?
                .to_string();

            Ok(self.admins.contains(&sender_uuid))
        }
    }

    /// Moves the character to the configured position and orientation.
    fn handle_position_and_orientation(&mut self) -> Result<(), String> {
        if let Some(current_position) = self.client.current::<Pos>() {
            if current_position != self.position {
                let entity = self.client.entity();
                let ecs = self.client.state_mut().ecs();
                let mut position_state = ecs.write_storage::<Pos>();

                position_state
                    .insert(entity, self.position)
                    .map_err(|error| error.to_string())?;
            }
        }

        if let Some(current_orientation) = self.client.current::<Ori>() {
            if current_orientation != self.orientation {
                let entity = self.client.entity();
                let ecs = self.client.state_mut().ecs();
                let mut orientation_state = ecs.write_storage::<Ori>();

                orientation_state
                    .insert(entity, self.orientation)
                    .map_err(|error| error.to_string())?;
            }
        }

        Ok(())
    }

    /// Gets the name of an item from its id.
    fn get_item_name(&self, item_id: ItemDefinitionId) -> String {
        let item =
            Item::new_from_item_definition_id(item_id, &self.ability_map, &self.material_manifest)
                .unwrap();
        let (item_name_i18n_id, _) = item.i18n(&self.item_i18n);

        self.localization.read().get_content(&item_name_i18n_id)
    }

    /// Finds the name of a player by their Uid.
    fn find_player_alias<'a>(&'a self, uid: &Uid) -> Option<&'a String> {
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

#[derive(Debug)]
pub struct Reciept {
    pub my_items: HashMap<String, u32>,
    pub their_items: HashMap<String, u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TradeMode {
    AdminAccess,
    Trade,
}

fn connect_to_veloren(
    game_server: String,
    auth_server: &str,
    username: &str,
    password: &str,
) -> Result<Client, String> {
    let runtime = Arc::new(Runtime::new().unwrap());
    let runtime2 = Arc::clone(&runtime);

    runtime
        .block_on(Client::new(
            ConnectionArgs::Tcp {
                hostname: game_server,
                prefer_ipv6: false,
            },
            runtime2,
            &mut None,
            username,
            password,
            None,
            |provider| provider == auth_server,
            &|_| {},
            |_| {},
            Default::default(),
            ClientType::Bot { privileged: false },
        ))
        .map_err(|error| format!("{error:?}"))
}
