# bid-scraper

bid-scraper is service that connects to relays using different methods and publishes the seen bids.
All the configuration comes from a config file so the command line is super simple:
```
bid-scraper config.toml
```

## Cfg

| Name | Type | Comments |
|------|------|-------------|
|log_json|bool|JSON vs Raw|
|log_level|env/string| Defines the log level (EnvFilter) for each mod. See https://docs.rs/tracing-subscriber/latest/tracing_subscriber/index.html for more info on this.<br>Example: "info"|
|log_color|bool||
|publisher_url|string|Where we publish the bids. Example:"tcp://0.0.0.0:5555"|

A list of publishers configurations follows. Each publisher is prefixed with ```[[publishers]]```.
Every publisher has the following fields:
| Name | Type | Comments |
|------|------|-------------|
|type|string|type of publisher to create.<br>Valid values:<br>- "relay-headers"<br>- "relay-bids"<br>- "ultrasound-ws"<br>- "bloxroute-ws"<br>See below for specific parameters for each type.|
|name|string|Unique name identifying this particular instance.Eg: "ultrasound-us","ultrasound-eu","relay-headers" |


The ```type``` field defines the specific publisher type 

### Publishers

* Relay pollers
    ##### Fields common to both publishers
    | Name | Type | Comments |
    |------|------|-------------|
    |eth_provider_uri|string|Endpoint for an EL client. Example:"ws://127.0.0.1:8545"|
    |relays_file|string|json file containing the list of relays. Example:"ws://127.0.0.1:8545"<br>Sample json file:<br><code>{<br>"flashbots": "https://0xac6e77dfe25ecd6110b8e780608cce0dab71fdd5ebea22a16c0205200f2f8e2e3ad3b71d3499c54ad14d6c21b41a37ae@boost-relay.flashbots.net",<br>"happy relay": "https://happy.com"<br>}</code>|
    |request_start_s|float|When should start to query (in seconds) for bids in each slot. It's then shifted using time_offset_index/time_offset_count. Example: 5.0|
    |request_interval_s|float|How often query for bids (in seconds), once we started. Example: 1.0|
    |time_offset_count|int|See time_offset_index. Example: 1|
    |time_offset_index|int|Int between [0; time_offset_count) . We'll initiate our requests at exactly this time proportionally in the slot. Imagine you have 3 instances in 3 servers, you pass --time-offset-count 3 and then the first instance will have --time-offset-index 0, the second 1, and the third 2.. Example: 0|
    
    * Relay get headers (type = "relay-headers") 
This publisher polls ```/eth/v1/builder/header/``` on the configured list of relays to get the current top bid.
Don't instantiate more than once.
        ##### Fields
        | Name | Type | Comments |
        |------|------|-------------|
        |beacon_node_uri|string| Endpoint for an CL client. Example:"ws://127.0.0.1:8545"|


    * Relay get bids (type = "relay-bids")
This publisher polls ```/relay/v1/data/bidtraces/builder_blocks_received``` on the configured list of relays to get all the bids.
Don't instantiate more than once.
No extra fields needed.


* Ultrasound websocket (type = "ultrasound-ws")
This publisher connects to a relay using ultrasound websocket top bid protocol.

    ##### Fields
    | Name | Type | Comments |
    |------|------|-------------|
    |ultrasound_url|string|Url to connect to. Example: "ws://relay-builders-eu.ultrasound.money/ws/v1/top_bid"|
    |relay_name|string|Be sure to use unique names. Maybe we can take it from the ultrasound_url?|
    |builder_id|optional string|Used as header X-Builder-Id, for use with ultrasound builder direct endpoint|
    |api_token|optional string|used as header X-Api-Token, for use with ultrasound builder direct endpoint|

* Bloxroute websocket (type = "bloxroute-ws")
This publisher connects to a relay using bloxroute websocket bids protocol.

    ##### Fields
    | Name | Type | Comments |
    |------|------|-------------|
    |bloxroute_url|string|Url to connect to. Example: "wss://mev-eth.blxrbdn.com/ws"|
    |relay_name| Be sure to use unique names. Maybe we can take it from the bloxroute_url?|
    |auth_header|string or env var|Added as "Authorization" header. Example: "env:BLOXROUTE_AUTH_HEADER"|

