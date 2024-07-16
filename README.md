# Veloren Trading Bot

A bot that buys, sells and trades with players.

The bot is containerized and can be run without compiling or building anything. Alternatively, you
can clone this repository and build the image yourself or build the binary directly with Cargo if
you are familiar with Rust.

## Warnings and Notices

- This project is **not officially supported** by the Veloren team. It will connect to the official
  Veloren server by default, but the moderators have the final say in whether it is allowed or
  not. **If you are asked not to use it, do not use it**.
- This is **not a cheat bot**. It does not give the player any advantage in the game. It is
  intended to be a fun addition to the game that can help players trade items with each other.
- This project may have bugs. You are encouraged to report them to the author but the author takes
  no responsibility for any lost items. **No such incidents have been reported so far.**
- This program **handles your password securely** and does nothing with it expect connecting to the
  veloren server during launch. However, third-party video game software is often infected with
  malware. You should **review the source code** or ask someone you trust to do so for you.
- You are welcome to make changes to the code or fork the project. The author is open to
  contributions and suggestions. But you **must indicate that the software has changed and
  distribute it under the same license**, which also requires it being open-source. You may not
  distribute the modified software as if it were the original.

## In-Game Commands

The bot is able to respond to the following commands, which must be sent via "/tell".

- `price [search term]`: Returns the buy/sell offers of any item whose name or ID contains the
  search term.
- `admin_access`: Admin-only, prompts the bot to send a trade invite to the sender, after which it
  will give away and accept any items until the trade ends.
- `announce`: Admin-only, sends the announcement message to "/world". This will reset the
  announcement timer to 45 minutes.
- `sort [count (optional)]`: Admin-only, sorts the inventory once or the given number of times.
- `pos [x] [y] [z]`: Admin-only, sets the bot's desired position where it will try to stand (must
  be close to the bot's current position)
- `ori [0-360]`: Admin-only, sets the bot's desired orientation (or facing direction)

## Prerequisites

You must have either [Docker](docker.com) or [Podman](podman.io) installed.

## Usage

All of these steps can be done with Docker but Podman is shown for the examples. If you use
Docker, just replace "podman" with "docker" in your commands.

### Setup

Create a "secrets.toml" file:

```toml
# secrets.toml
username = "bot_username"
password = "bot_password"
character = "bot_character"

# You may add usernames or UUIDs to this list. The only advantage of using UUIDs is that it will
# persist accross changes to your username or player alias.
admins = ["my_username"]
```

Then create a secret to pass the file securely to the container.

```sh
podman secret create secrets.toml secrets.toml
```

You will also need a "config.toml" and it needs it be in a "config" directory that can be mounted
to the container:

```toml
# config/config.toml

# Optional. The bot will connect to the official server if this is not set.
game_server = "server.veloren.net"

# Optional. The bot will connect to the official auth server if this is not set. Be careful
# if you set this, your username and password will be sent to this server. Most servers use the
# official auth server so you can probably leave this out, even if you are using an alternate
# game server.
auth_server = "https://auth.veloren.net"

# Optional. Change these to the desired X, Y, Z coordinates. The bot will try to stand here, but
# the coordinates must be close to the bot's spawn point. If not set, the bot will stand at its
# spawn point. Its position can be changed in-game with the "pos" command.
position = [0, 0, 0]

# Optional. (0 = North, 90 = West, 180 = South, 270 = East) If not set, the bot will face North.
# Its orientation can be changed in-game with the "ori" command.
orientation = 0

# Optional. Announcements are sent every 45 minutes. Use {location} to insert the bot's current
# location. If not set, the bot will not send /world announcements but will still send /region
# announcements with usage instructions.
announcement = "I love cheese! I am at {location}."

# The buy_prices and sell_prices tables are required. The keys are item definition IDs and the
# values are the price in coins. You may type in-game "/give_item common.items." and press Tab to
# explore the item definition IDs. Then just leave off the "common.items." part in this file.

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
    --volume ./config/:/root/config/ \
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

Then follow the [above](#running) steps with the tag "trade_bot" instead of
"git.jeffa.io/jeff/trade_bot".
