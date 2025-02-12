# About

`test-relay` is a dummy relay implementation for block builders that can be used for testing without submitting to the real MEV-boost relay.

To use test-relay, you need:
* A real MEV-boost relay URL
* A connection to a consensus layer node
* (optional) A validation endpoint to validate blocks

It provides the following API endpoints:
* GET /relay/v1/builder/validators
* POST /relay/v1/builder/blocks


Additionally, it exposes metrics, including estimated slot auction winners among builders who submit to this relay.
