# Veloren Trading Bot

A bot that buys, sells and trades with players.

The bot is containerized and can be run without compiling or building anything. Alternatively, you can clone this repository and build the image yourself or build the binary directly with Cargo if you are familiar with Rust.

## Prerequisites

You must have either [Docker](docker.com) or [Podman](podman.io) installed.

## Usage

All of these steps can be done with Docker but Podman is shown for the examples. If you use Docker, just replace "podman" with "docker" in your commands.

### Setup

Create a "secrets.toml" file:

```toml
# secrets.toml
username = "bot_username"
password = "bot_password"
character = "bot_character"
admins = ["my_username"]
```

Then create a secret to pass the file securely to the container.

```sh
podman secret create secrets.toml secrets.toml
```

You will also need a "config.toml" and it needs it be in a "config" directory that can be mounted to the container:

```toml
# config/config.toml
position = [0, 0, 0] # Change these to the desired X, Y, Z coordinates
orientation = 0 # 0 = North, 90 = West, 180 = South, 270 = East
announcement = "Crabo loves cheese! I am " # The bot appends "at [location]." to each announcement.

[buy_prices]
"food.cheese" = 50

[sell_prices]
"consumable.potion_minor" = 150
```

### Running

Run the container:

```sh
podman run \
    --detach \
    --name trade_bot \
    --secret secrets.toml \
    -v ./config/:/root/config/ \
    --env CONFIG=/root/config/config.toml \
    --env SECRETS=/run/secrets/secrets.toml \
    --env RUST_LOG=trade_bot \
    git.jeffa.io/jeff/trade_bot
```

View the log output with `podman logs -f trade_bot`.

### Building

Clone this repository. From the project root:

```sh
podman build . -t trade_bot
```

Then follow the [above](#running) steps with the tag "trade_bot" instead of "git.jeffa.io/jeff/trade_bot".

### In-Game Commands

The bot is able to respond to the following commands, which must be sent via "/tell".

- `price [search term]`: Returns the buy/sell offers of any item whose item definition ID contains the search term.
- `admin_access`: Admin-only, prompts the bot to send a trade invite, after which it will give away and accept any items until the trade ends.
- `sort [count (optional)]`: Admin-only, sorts the inventory once or the given number of times.
- `position [x] [y] [z]`: Admin-only, sets the bot's desired position where it will try to stand (must be close to the character)
- `orientation [0-360]`: Admin-only, sets the bots' desired orientation (or facing direction)
