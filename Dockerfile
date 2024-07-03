FROM rust:1-slim AS build

WORKDIR /app

COPY Cargo.toml .
COPY Cargo.lock .
COPY rust-toolchain .
COPY src/ src/

RUN apt-get update
RUN apt-get install -y git
RUN cargo build --release

FROM fedora:40

WORKDIR /app

COPY --from=build /app/target/release/group-bot group-bot
COPY assets/ assets/

RUN chmod +x group-bot

CMD ["./group-bot"]
