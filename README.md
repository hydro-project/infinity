# Infinity Agents

This project is a proof-of-concept of Infinity Agents: a new runtime and architecture for agents that can run indefinitely with zero resource usage when they are idle.

## Prerequisites

- [Rust](https://www.rust-lang.org/tools/install)
- [Cargo Lambda](https://www.cargo-lambda.info/guide/installation.html)
- [Node.js](https://nodejs.org/) (for CDK deployment)
- [AWS CDK](https://docs.aws.amazon.com/cdk/v2/guide/getting_started.html)

## Deployment

This project uses AWS CDK for infrastructure deployment. See the [CDK README](cdk/README.md) for detailed setup and deployment instructions.

Quick start:

1. Configure environment variables:
```bash
cp cdk/.env.example cdk/.env
# Edit cdk/.env with your credentials
```

2. Build and deploy:
```bash
# Build the Rust Lambda
cargo lambda build --release --arm64

# Deploy infrastructure (fish shell)
cd cdk
chmod +x deploy.fish
./deploy.fish

# Or for bash/zsh
cd cdk
source ../.env && npx cdk deploy
```

For more details on the infrastructure, configuration, and testing, see the [CDK README](cdk/README.md).
