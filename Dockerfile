# syntax=docker/dockerfile:1
FROM --platform=$BUILDPLATFORM node:24-bookworm-slim AS web
WORKDIR /src/web
COPY web/package*.json ./
RUN npm ci
COPY web/ ./
RUN npm run build

FROM rust:bookworm AS build
ARG STARSTACK_FEATURES=""
WORKDIR /src
COPY . .
COPY --from=web /src/web/dist ./web/dist
RUN if [ -n "$STARSTACK_FEATURES" ]; then apt-get update && apt-get install -y --no-install-recommends cmake && rm -rf /var/lib/apt/lists/*; fi
RUN cargo build --release --locked --bin starstack --features "$STARSTACK_FEATURES"

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && install -d -m 0750 -o 10001 -g 10001 /data
COPY --from=build /src/target/release/starstack /usr/local/bin/starstack
ENV STARSTACK_HOME=/data
USER 10001:10001
VOLUME ["/data"]
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/starstack"]
CMD ["serve", "--bind", "0.0.0.0:8080"]
