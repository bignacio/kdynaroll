
FROM debian:trixie-slim

RUN apt-get update && apt-get install -y ca-certificates libssl3 && rm -rf /var/lib/apt/lists/*

COPY kdynaroll /usr/local/bin/kdynaroll

ENTRYPOINT ["/usr/local/bin/kdynaroll"]
