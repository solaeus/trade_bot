# Veloren Trading Bot

A bot that buys, sells and trades with players.

## Usage

All of these steps can be done with docker instead of podman.

### Running from pre-built image

#### Setup

Create a "secrets.toml" file:

```toml
# secrets.toml
username = "my_username"
password = "my_password"
```

Then create a secret to pass the file securely to the container.

```sh
podman secret create secrets.toml secrets.toml
  
```

You will also need a "config.toml":

```toml
# config.toml
character = "my_character"
position = [0.0, 0.0, 0.0] # Change these to the desired X, Y, Z coordinates
orientation = "West"

[buy_prices]
"common.items.food.cheese" = 50

[sell_prices]
"common.items.armor.boreal.back" = 250_000
```

Place this config file inside a directory called "config".

#### Running

```sh
podman run \
    --secret secrets.toml \
    -v ./config/:/root/config/ \
    --env CONFIG=/root/config/config.toml \
    --env SECRETS=/run/secrets/secrets.toml \
    --env RUST_LOG=trade_bot \
    git.jeffa.io/jeff/trade_bot
```

### Building

From the directory root:

```sh
podman build . -t trade_bot
```

Then follow the [above](#Running_from_pre-built_image) steps with the tag "trade_bot" instead of
"git.jeffa.io/jeff/trade_bot".
