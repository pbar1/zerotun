FROM lukemathwalker/cargo-chef:latest-rust-trixie AS chef
WORKDIR /build

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --locked --bin zerotun --recipe-path recipe.json
COPY . .
RUN cargo build --release --locked --bin zerotun

FROM gcr.io/distroless/cc-debian13:nonroot
LABEL org.opencontainers.image.source="https://github.com/pbar1/zerotun"
LABEL org.opencontainers.image.description="TCP and UDP reverse tunnel over QUIC."
COPY --from=build /build/target/release/zerotun /zerotun
USER 65532:65532
ENTRYPOINT ["/zerotun"]
