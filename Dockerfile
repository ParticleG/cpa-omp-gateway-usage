ARG RUST_IMAGE=rust:bookworm
FROM ${RUST_IMAGE} AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
COPY web ./web
ARG VERSION=dev
ENV PLUGIN_VERSION=${VERSION}
ENV RUSTFLAGS="--remap-path-prefix=/src=."
RUN cargo test --locked && cargo build --release --locked

FROM scratch
COPY --from=build /src/target/release/libomp_gateway_usage.so /omp-gateway-usage.so
