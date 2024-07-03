use std::{
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use serde::Serialize;
use tokio::runtime::Runtime;
use veloren_client::{addr::ConnectionArgs, Client, Event};
use veloren_common::{
    clock::Clock,
    comp::{
        invite::InviteKind,
        item::{ItemDefinitionId, ItemDefinitionIdOwned, ItemDesc},
        ChatType, ControllerInputs, Item,
    },
    trade::TradeAction,
    uid::Uid,
    ViewDistances,
};
use veloren_common_net::{msg::InviteAnswer, sync::WorldSyncExt};

pub struct Bot {
    client: Client,
    clock: Clock,
    last_announcement: Instant,
}

impl Bot {
    pub fn new(username: &str, password: &str) -> Result<Self, String> {
        let client = connect_to_veloren(username, password)?;
        let clock = Clock::new(Duration::from_secs_f64(1.0 / 60.0));

        Ok(Bot {
            client,
            clock,
            last_announcement: Instant::now(),
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

    pub fn announce(&mut self) {
        self.client.send_command(
            "say".to_string(),
            vec![
                "Buying cheese! Type '/say trade' near me and place your cheese in the window."
                    .to_string(),
            ],
        );
    }

    pub fn tick(&mut self) -> Result<(), String> {
        let events = self
            .client
            .tick(ControllerInputs::default(), self.clock.dt())
            .map_err(|error| format!("{error:?}"))?;

        for event in events {
            self.handle_event(event)?;
        }

        let now = Instant::now();

        if now.duration_since(self.last_announcement) > Duration::from_secs(100) {
            self.announce();

            self.last_announcement = now;
        }

        self.handle_trading()?;
        self.client.cleanup();
        self.clock.tick();

        Ok(())
    }

    fn handle_event(&mut self, event: Event) -> Result<(), String> {
        match event {
            Event::Chat(message) => match message.chat_type {
                ChatType::Tell(sender_uid, _) | ChatType::Say(sender_uid) => {
                    if message.content().as_plain() == Some("trade") && !self.client.is_trading() {
                        self.client.send_invite(sender_uid, InviteKind::Trade);
                    }
                }
                _ => {}
            },
            Event::InviteComplete {
                target,
                answer,
                kind,
            } => {
                if let InviteKind::Trade = kind {
                    if let InviteAnswer::Accepted = answer {
                        if let Some(name) = self.find_name(&target) {
                            self.client.send_command(
                                "say".to_string(),
                                vec![format!("Trading with {name}")],
                            );
                        }
                    }
                }
            }
            _ => {}
        }

        Ok(())
    }

    fn handle_trading(&mut self) -> Result<(), String> {
        if let Some((_, trade, _)) = self.client.pending_trade() {
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
                            acc + (get_item_value(item) * quantity)
                        } else {
                            acc
                        }
                    });
            let my_offered_coins = (&trade.offers[0])
                .into_iter()
                .find_map(|(slot_id, quantity)| {
                    let item = my_inventory.get(slot_id.clone()).unwrap();

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

            thread::sleep(Duration::from_millis(300));
        }

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

fn get_item_value(item: &Item) -> u32 {
    match item.name().as_ref() {
        "Dwarven Cheese" => 50,
        _ => 0,
    }
}
