# infinity-agents-cdk

AWS CDK constructs for deploying [Infinity](https://infinity.hydro.run) agents on AWS Lambda, plus an
example CDK app that uses them. See the
[Serverless Deployments docs](https://infinity.hydro.run/docs/infinity-runtime/serverless/quickstart)
for the full guide.

## Using the constructs in your own CDK app

The package is installed straight from this repository as a git dependency. pnpm is required (it
supports installing from a subdirectory of a git repo):

```bash
pnpm add "github:hydro-project/infinity#path:agent"
```

The install runs this package's `prepare` script, which compiles the constructs and vendors the
Rust workspace into the package so the agent Lambda can be built outside the repo (locally via
[cargo-lambda](https://www.cargo-lambda.info/), or automatically in Docker).

```typescript
import { InfinityAgent, RapToolSet, HTTPMCPToolSet, LambdaMCPToolSet, SlackIntegration } from 'infinity-agents-cdk';

const agent = new InfinityAgent(this, 'Agent');
```

The library source lives in [`lib/infinity-agents/`](lib/infinity-agents/). The `InfinityAgent`
construct provisions the leader Lambda (the Rust runtime in `crates/infinity-agent-lambda`), the
SQS FIFO input queue, the output queue, the Aurora DSQL conversation store, the DynamoDB state
table, the RAP callback receiver, and durable sleep timers.

## Developing the example agent in this repo

[`lib/example-agent.ts`](lib/example-agent.ts) is a full agent wired up with the example tool sets
in [`lib/toolsets/`](lib/toolsets/) (time, EC2, GitHub webhooks, finance subscriptions, code
sandboxes) and the Slack integration.

1. Configure environment variables:

```bash
# Copy the example file and fill in your Slack/GitHub credentials
cp .env.example .env
```

2. Install dependencies:

```bash
npm install
```

3. Deploy the stack:

**For bash/zsh:**
```bash
npx cdk bootstrap  # Only needed once per account/region
source .env && npx cdk deploy
```

**For fish shell:**
```bash
npx cdk bootstrap  # Only needed once per account/region
./deploy.fish
```
