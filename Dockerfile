# craft-live: a static binary on an empty image.
#   docker build -t craft-live .
#   docker run -p 9980:9980 -v craft-live-data:/data -v /srv/craft-apps:/apps:ro \
#     -e CRAFT_LIVE_PUBLIC_URL=https://live.example.com -e CRAFT_LIVE_WOPI_HOSTS=https://cloud.example.com craft-live
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p craft-live \
 && strip target/release/craft-live \
 && mkdir -p /out/data && cp target/release/craft-live /out/

FROM alpine:3 AS certs
RUN apk add --no-cache ca-certificates

FROM scratch
COPY --from=certs /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /out/craft-live /craft-live
COPY --from=build --chown=10001:10001 /out/data /data
ENV CRAFT_LIVE_LISTEN=0.0.0.0:9980 \
    CRAFT_LIVE_DATA_DIR=/data \
    CRAFT_LIVE_APP_GRIDCRAFT=/apps/gridcraft \
    CRAFT_LIVE_APP_WORDCRAFT=/apps/wordcraft \
    CRAFT_LIVE_APP_DECKCRAFT=/apps/deckcraft \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
USER 10001:10001
VOLUME /data
EXPOSE 9980
ENTRYPOINT ["/craft-live"]
