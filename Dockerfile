# Catatan: di Apple Silicon build dengan `--platform linux/amd64`.
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev gcc make
RUN rustup target add x86_64-unknown-linux-musl
WORKDIR /src

# Cache dependensi: build dulu dengan main kosong.
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --target x86_64-unknown-linux-musl \
    && rm -rf src

COPY src ./src
RUN touch src/main.rs && cargo build --release --target x86_64-unknown-linux-musl

FROM scratch
COPY --from=build /src/target/x86_64-unknown-linux-musl/release/simple-api-gateway /gateway
USER 65534:65534
EXPOSE 3000
ENTRYPOINT ["/gateway"]
