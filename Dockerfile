# syntax=docker/dockerfile:1

# One image, three binaries: invoice-service, mock-psp, webhook-sink.
# docker-compose.yml picks which one each container runs.

FROM rust:1.93-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bins \
 && mkdir -p /out \
 && cp target/release/invoice-service target/release/mock-psp target/release/webhook-sink /out/

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 app
COPY --from=build /out/ /usr/local/bin/
USER app
CMD ["invoice-service", "serve"]
