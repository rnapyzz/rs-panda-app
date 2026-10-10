FROM rust:1.90-bookworm AS build
WORKDIR /src
# 依存 crate のビルドをキャッシュする（ソースを変えても依存は再ビルドしない）
COPY Cargo.toml Cargo.lock build.rs ./
COPY migrations ./migrations
RUN mkdir -p src/bin && echo 'fn main() {}' > src/bin/server.rs && echo 'fn main() {}' > src/bin/migrate.rs \
 && echo 'fn main() {}' > src/bin/createuser.rs && touch src/lib.rs \
 && cargo build --release --locked && rm -rf src
COPY . .
RUN touch src/lib.rs src/bin/*.rs && cargo build --release --locked \
 && mkdir -p /out && cp target/release/server target/release/migrate target/release/createuser /out/

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates tzdata \
 && rm -rf /var/lib/apt/lists/* && useradd -u 10001 -M app
COPY --from=build /out/server /out/migrate /out/createuser /usr/local/bin/
USER app
EXPOSE 8080
CMD ["server"]
