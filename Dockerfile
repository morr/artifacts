# syntax=docker/dockerfile:1
FROM rust:1-alpine AS build
WORKDIR /app
RUN apk add --no-cache musl-dev
# Cache dependencies: build a stub first, then overlay the real sources.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src \
    && mkdir src
COPY src ./src
RUN touch src/main.rs && cargo build --release

FROM scratch
COPY --from=build /app/target/release/artifacts-server /artifacts-server
ENV ARTIFACTS_DATA_DIR=/data PORT=8080
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["/artifacts-server"]
