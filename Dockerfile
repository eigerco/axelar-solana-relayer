# ==== BUILD STAGE ====
FROM rust:latest AS builder

RUN rustup toolchain install nightly && rustup default nightly

# Set working directory inside the container
WORKDIR /app

# Copy source code and build
COPY . .
#RUN cargo build --release disable release for now ...
RUN cargo build

# ==== RUN STAGE ====
FROM debian:bookworm-slim AS runtime

ENV CONFIG=/app/config.toml

# Install necessary runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Set a non-root user for security
RUN useradd -m rustuser
USER rustuser

WORKDIR /home/rustuser

# Copy compiled binary from builder stage
COPY --from=builder /app/config.toml /app/config.toml
COPY --from=builder /app/target/debug/solana-axelar-relayer /usr/local/bin/solana-axelar-relayer

# Set the entrypoint
ENTRYPOINT ["/usr/local/bin/solana-axelar-relayer"]
