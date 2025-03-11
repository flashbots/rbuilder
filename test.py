import asyncio
import json
from pprint import pprint

import websockets
from eth_account import Account
from eth_account.signers.local import LocalAccount
from flashbots import flashbot
from web3 import HTTPProvider, Web3
from web3.types import TxParams
import time

# Initialize signer
signer1: LocalAccount = Account.from_key("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80")
signer2: LocalAccount = Account.from_key("0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d")
signer3: LocalAccount = Account.from_key("0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a")
signer4: LocalAccount = Account.from_key("0x47e179ec197488593b187f80a00eb0da91f1b9d0b13f8733639f19c30a34926a")

# HTTP connection for getting nonce and other information
w3 = Web3(HTTPProvider("http://localhost:8545"))
w3flash = Web3(HTTPProvider("http://localhost:8645"))
flashbot(w3flash, signer1, "http://localhost:8645")

# WebSocket endpoint (update with your WebSocket URL)
websocket_url = "ws://localhost:3030"

def get_nonce(signer=signer1):
    return w3.eth.get_transaction_count(signer.address)

# Transaction parameters
def tx(value, gas, priority, nonce, signer=signer1):
    x: TxParams = {
        "to": "0xa0Ee7A142d267C1f36714E4a8F75612F20a79720",
        "value": Web3.to_wei(value, "ether"),
        "gas": gas,
        "maxFeePerGas": Web3.to_wei(1000, "gwei"),
        "maxPriorityFeePerGas": Web3.to_wei(priority, "gwei"),
        "nonce": nonce,
        "chainId": 1337,
        "type": 2,
    }
    return signer.sign_transaction(x)

# Send transaction via WebSocket
async def send_transaction(ws_url, payload):
    async with websockets.connect(ws_url) as websocket:
        # Create a JSON-RPC request
        request = {
            "action": "submit_bid",
            "transaction": payload,
        };

        # Send the request as JSON
        payload = json.dumps(request)

        await websocket.send(payload)
        print(f"Sent transaction to WebSocket: {payload}")

        # nonce1 = get_nonce(signer1)
        # nonce2 = get_nonce(signer2)
        # w3flash.eth.send_raw_transaction(tx(5, 21000, 100, nonce1, signer1).rawTransaction)
        # w3flash.eth.send_raw_transaction(tx(20, 21000, 50, nonce2, signer2).rawTransaction)
        # print("Sent 2 tx")
        #
        # print("Sleeping for 2 seconds...")
        # time.sleep(0.5)
        # w3flash.eth.send_raw_transaction(tx(51, 21000, 101, 1, signer4).rawTransaction)


async def main():
    nonce3 = get_nonce(signer3)
    serialized_tx = tx(666, 23000, 200, nonce3, signer3).rawTransaction.hex()
    await send_transaction(websocket_url, serialized_tx)

asyncio.run(main())


# nonce1 = get_nonce(signer1)
# nonce2 = get_nonce(signer2)
# w3flash.eth.send_raw_transaction(tx(5, 21000, 100, nonce1, signer1).rawTransaction)
# w3flash.eth.send_raw_transaction(tx(20, 21000, 50, nonce2, signer2).rawTransaction)


# block = w3.eth.get_block(3, full_transactions=True)
# pprint(block.transactions)
