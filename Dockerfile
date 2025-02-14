FROM debian:buster-slim AS chef 
RUN apt-get update && \
    export DEBIAN_FRONTEND=noninteractive && \
    apt-get install -yq \
    build-essential \
    cmake \
    clang \ 
    curl \
    protobuf-compiler
ENV RUSTUP_HOME=/opt/rust/rustup \
    PATH=/home/root/.cargo/bin:/opt/rust/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
RUN curl https://sh.rustup.rs -sSf | \
    env CARGO_HOME=/opt/rust/cargo \
    sh -s -- -y --default-toolchain stable --profile minimal --no-modify-path && \
    env CARGO_HOME=/opt/rust/cargo \
    rustup component add rustfmt
RUN env CARGO_HOME=/opt/rust/cargo cargo install cargo-chef && \
    rm -rf /opt/rust/cargo/registry/
WORKDIR /app

FROM chef AS planner
COPY Cargo.toml .
COPY Cargo.lock .
COPY crates/ crates/
COPY resources/ resources/
COPY tests/ tests/
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder 
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY Cargo.toml .
COPY Cargo.lock .
COPY crates/ crates/
COPY resources/ resources/
COPY tests/ tests/
RUN cargo build --manifest-path=crates/main/Cargo.toml --release
RUN cargo build --manifest-path=crates/install/Cargo.toml --release

FROM debian:buster-slim AS runtime

COPY --from=builder /app/target/release/stalwart-mail /usr/local/bin/stalwart-mail
COPY --from=builder /app/target/release/stalwart-install /usr/local/bin/stalwart-install
RUN apt-get update -y && apt-get install -yq ca-certificates
RUN echo "#\!/bin/sh\n\n/usr/local/bin/stalwart-install -c all-in-one -p /opt/stalwart-mail -d" > /usr/local/bin/configure.sh && \
    chmod +x /usr/local/bin/configure.sh
RUN useradd stalwart-mail -s /sbin/nologin -M
RUN mkdir -p /opt/stalwart-mail
RUN chown stalwart-mail:stalwart-mail /opt/stalwart-mail

VOLUME [ "/opt/stalwart-mail" ]

ENTRYPOINT ["/usr/local/bin/stalwart-mail", "--config", "/opt/stalwart-mail/etc/config.toml"]
