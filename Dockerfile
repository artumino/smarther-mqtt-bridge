FROM rust:latest AS builder
ENV CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse
RUN update-ca-certificates
WORKDIR /smarther-mqtt-bridge
COPY ./ .
RUN cargo build --release

FROM gcr.io/distroless/cc
WORKDIR /app
COPY --from=builder /smarther-mqtt-bridge/target/release/smarther-mqtt-bridge ./
CMD ["/app/smarther-mqtt-bridge", "run"]