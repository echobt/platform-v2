# Platform

Distributed validator network for decentralized AI evaluation on Bittensor.

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                              SUDO OWNER                                      │
│                    (Creates, Updates, Removes Challenges)                    │
└─────────────────────────────────┬───────────────────────────────────────────┘
                                  │
                                  ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                         SHARED BLOCKCHAIN STATE                              │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐        │
│  │ Challenge A │  │ Challenge B │  │ Challenge C │  │     ...     │        │
│  │ (Docker)    │  │ (Docker)    │  │ (Docker)    │  │             │        │
│  └─────────────┘  └─────────────┘  └─────────────┘  └─────────────┘        │
│                                                                              │
│  ┌──────────────────────────────────────────────────────────────────────┐  │
│  │                     Distributed Database (Merkle Trie)                │  │
│  │              Evaluation Results • Scores • Agent Data                 │  │
│  └──────────────────────────────────────────────────────────────────────┘  │
└─────────────────────────────────┬───────────────────────────────────────────┘
                                  │
            ┌─────────────────────┼─────────────────────┐
            │                     │                     │
            ▼                     ▼                     ▼
┌───────────────────┐ ┌───────────────────┐ ┌───────────────────┐
│   VALIDATOR 1     │ │   VALIDATOR 2     │ │   VALIDATOR N     │
├───────────────────┤ ├───────────────────┤ ├───────────────────┤
│ ┌───────────────┐ │ │ ┌───────────────┐ │ │ ┌───────────────┐ │
│ │ Challenge A   │ │ │ │ Challenge A   │ │ │ │ Challenge A   │ │
│ │ Challenge B   │ │ │ │ Challenge B   │ │ │ │ Challenge B   │ │
│ │ Challenge C   │ │ │ │ Challenge C   │ │ │ │ Challenge C   │ │
│ └───────────────┘ │ │ └───────────────┘ │ │ └───────────────┘ │
│                   │ │                   │ │                   │
│ Evaluates agents  │ │ Evaluates agents  │ │ Evaluates agents  │
│ Shares results    │ │ Shares results    │ │ Shares results    │
└─────────┬─────────┘ └─────────┬─────────┘ └─────────┬─────────┘
          │                     │                     │
          └─────────────────────┼─────────────────────┘
                                │
                                ▼
                    ┌───────────────────────┐
                    │   BITTENSOR CHAIN     │
                    │   (Weight Submission) │
                    │  Each Epoch (~72min)  │
                    └───────────────────────┘
```

## How It Works

1. **Sudo owner** registers challenges (Docker images) on the network
2. **All validators** pull and run the same challenge containers
3. **Agents submit** to any validator, results sync across the network
4. **Validators share** evaluation data via P2P (libp2p gossipsub)
5. **At each epoch**, validators aggregate scores → submit weights to Bittensor

All validators execute all challenges identically. Consensus requires matching state.

## Quick Start

```bash
git clone https://github.com/PlatformNetwork/platform.git
cd platform
cp .env.example .env
# Edit .env: add your VALIDATOR_SECRET_KEY (BIP39 mnemonic)
docker compose up -d
```

The validator will auto-connect to `bootnode.platform.network` and sync.

## Auto-Update (Critical)

**All validators MUST use auto-update.** Watchtower checks for new images at synchronized times (`:00`, `:05`, `:10`, `:15`...) so all validators update together.

If validators run different versions:
- Consensus fails (state hash mismatch)
- Weight submissions rejected
- Network forks

**Do not disable Watchtower.** All validators must update at the same time to maintain consensus.

## Configuration

| Variable | Description | Required |
|----------|-------------|----------|
| `VALIDATOR_SECRET_KEY` | BIP39 mnemonic or hex key | Yes |
| `RUST_LOG` | Log level (default: `info`) | No |

## RPC API

```bash
curl -X POST http://localhost:8080/rpc \
  -d '{"jsonrpc":"2.0","method":"system_health","id":1}'
```

## License

MIT
