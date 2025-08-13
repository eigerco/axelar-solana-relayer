# Solana Axelar Relayer

This repository contains the Relayer that allows the Solana Gateway to communicate with Axelar Amplifier API.

## Table of Contents

- [Repository contents](#repository-contents)
  - [Components](#components)
  - [Utility crates](#utility-crates)
  - [Related repositories](#related-repositories)
- [Getting Started](#getting-started)
  - [Prerequisites](#prerequisites)
  - [Installation](#installation)

## Repository contents

![High-level overview](https://github.com/user-attachments/assets/c2403e07-b1c1-417e-a926-963554a69f9a)

The relayer is built of composable plug-n-play items, called `components`, which can communicate with each other via channels and (optionally) persist in their storage.

[**Project entrypoint main.rs**](crates/solana-axelar-relayer).

### Components
- [**Solana Listener**](crates/solana-listener): Receives transaction log data from Solana using WebSockets. Listens to Gas Service and the Gateway.
- [**Solana Event Forwarder**](crates/solana-event-forwarder): Parses transaction logs into events and combines the Gas Service events with the Gateway logs. Forwards them to the Amplifier component.
- [**Rest Service**](crates/rest-service): Receives an off-chain payload from the end-user, combines it with an on-chain event and sends it to the Amplifier API.
- [**Solana Gateway Task Processor**](crates/solana-gateway-task-processor): Receives tasks from Amplifier API and composes transactions to interact with the Gateway and the destination contract.

### Utility crates
- [**File-based storage**](crates/file-based-storage): Simple file-based memory-mapped buffer storage for tracking metadata for different components.
- [**Retrying Solana HTTP Sender**](crates/retrying-solana-http-sender): Solana RPC client wrapped with a retrying mechanism to account for Solana node fluctuations. Used by all components that require Solana RPC access.
- [**Effective TX Sender**](crates/effective-tx-sender): Evaluate the necessary compute budget and unit price and prefix this new instruction inside the transaction.
- [**Gateway Gas Computation**](crates/gateway-gas-computation): The Axelar Solana Gateway has a multi-computation model; therefore, figuring out how much money was spent on a single gateway action needs to be summed together based on all the actions that the relayer did across many transactions.


## Related Repositories

- [**Solana Contracts**](https://github.com/eigerco/solana-axelar): The Solana on-chain contracts and home to some of the utillty crates used in this repository.
- [**Relayer Core**](https://github.com/eigerco/axelar-relayer-core): All Axelar-related relayer infrastructure. Used as a core building block for the Solana Relayer. The Axelar-Starknet and Axlelar-Aleo relayers also use it.

## Getting Started

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install)
- [Solana CLI (for running tests during development)](https://solana.com/docs/intro/installation)
- [Surfpool](https://github.com/txtx/surfpool) (for gas cost estimation) [(docker image)](https://hub.docker.com/r/surfpool/surfpool).

### Surfpool Dependency

This relayer depends on [Surfpool](https://hub.docker.com/r/surfpool/surfpool), a Solana state forking service that enables accurate gas cost estimation by running transactions on forked cluster state. Surfpool is used by the [Solana Gateway Task Processor](crates/solana-gateway-task-processor) component for estimating execution costs.

#### Running Surfpool with Docker

An official Docker image is available at `surfpool/surfpool`. Here's how to run it:

```bash
# Run Surfpool for devnet
docker run -p 8899:8899 surfpool/surfpool:latest start --slot-time 1 --no-tui -n devnet

# Run Surfpool for testnet
docker run -p 8899:8899 surfpool/surfpool:latest start --slot-time 1 --no-tui -n testnet

# Run Surfpool for mainnet
docker run -p 8899:8899 surfpool/surfpool:latest start --slot-time 1 --no-tui -n mainnet
```

#### Architecture Compatibility

**Important**: Currently, only ARM64 Docker images are available for Surfpool. If you're running on a different architecture (x86_64, etc.), you'll need to use QEMU emulation:

1. Install `qemu-user-static` for cross-architecture support:
   ```bash
   # On Ubuntu/Debian
   sudo apt-get install qemu-user-static

   # On macOS
   brew install qemu
   ```

2. Enable multi-architecture support:
   ```bash
   docker run --rm --privileged multiarch/qemu-user-static --reset -p yes
   ```

3. Run Surfpool with platform specification:
   ```bash
   # For devnet
   docker run --platform linux/arm64 -p 8899:8899 surfpool/surfpool:latest start --slot-time 1 --no-tui -n devnet

   # For mainnet
   docker run --platform linux/arm64 -p 8899:8899 surfpool/surfpool:latest start --slot-time 1 --no-tui -n mainnet
   ```

For more information about QEMU user static emulation, see the [official repository](https://github.com/multiarch/qemu-user-static).

### Installation

```bash
git clone git@github.com:eigerco/axelar-solana-relayer.git
cargo xtask test
cargo xtask --help # see what else you can do

# Confgure the config
cp config.example.toml config.toml
# Run the relayer
cargo run -p axelar-solana-relayer
```

## About [Eiger](https://www.eiger.co)

We are engineers. We contribute to various ecosystems by building low-level implementations and core components. We work on several Axelar and Solana projects, and connecting these two is a fundamental goal to achieve cross-chain execution.

Contact us at hello@eiger.co
Follow us on [X/Twitter](https://x.com/eiger_co)
