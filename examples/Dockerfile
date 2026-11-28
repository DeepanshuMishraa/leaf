FROM rust:1.85 AS build
WORKDIR /app
COPY . .
RUN cargo build --release --locked

FROM debian:bookworm-slim
COPY --from=build /app/target/release/leaf /usr/local/bin/leaf
ENTRYPOINT ["leaf"]
