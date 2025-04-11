# Cronjob

## Running with builder playground

```
git clone https://github.com/flashbots/builder-playground
cd builder-playground
# this branch contains funded accounts with anvil defaults
git checkout cron-job-hack

go run main.go cook opstack --external-builder http://host.docker.internal:4444
```

### Running with op-rbuilder

```
# to get a clean slate
rm -rf /tmp/builder

# clone op-rbuilder
git clone https://github.com/flashbots/op-rbuilder
cd op-rbuilder
git checkout cron-job-hack

cargo run -p op-rbuilder --bin op-rbuilder -- node   --rollup.sequencer-http http://localhost:8549  --rollup.builder-secret-key 0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d --authrpc.port 4444 --authrpc.jwtsecret ~/.playground/devnet/jwtsecret --http --http.port 8645 --chain ~/.playground/devnet/l2-genesis.json --datadir /tmp/builder --disable-discovery --port 30333 --trusted-peers enode://3479db4d9217fb5d7a8ed4d61ac36e120b05d36c2eefb795dc42ff2e971f251a2315f5649ea1833271e020b9adc98d5db9973c7ed92d6b2f1f2223088c3d852f@127.0.0.1:30304
```

### Deploying the contracts

```
git clone https://github.com/Melvillian/cronjob.git

forge build

forge script --chain 13 script/CronInbox.s.sol:CronInboxScript --rpc-url http://localhost:8546/  --broadcast -vvvv --interactives 1

# use the default anvil private key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80

forge script --chain 13 script/Increment.s.sol:IncrementScript --rpc-url localhost:8546 --broadcast -vvvv --interactives 1

# update the contract address in the script
# i.e. cronInboxAddress - 0x5FbDB2315678afecb367f032d93F642f64180aa3
# increment - 0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512
forge script --chain 13 script/CreateCronJob.s.sol:CreateCronJobScript --rpc-url localhost:8546 --broadcast -vvvv --interactives 1
```

## External network

If using a different network, update the [contract address](https://github.com/flashbots/rbuilder/blob/cron-job-hack/crates/op-rbuilder/src/cronjob.rs#L49) in the builder and the funded builder signer key to send the transactions i.e. `--rollup.builder-secret-key` variable in the builder.

Update the addresses in the create cron job solidity script for the cronInbox and increment contract as well
