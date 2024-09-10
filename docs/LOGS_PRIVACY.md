# Logs Privacy in rbuilder

## Introduction

Log privacy in rbuilder refers to the level of data exposed in logs via macros like `error!`. A key principle in rbuilder is that we never log a full order, as this information could be harmful to the order sender.

### Why is this important?

- A non-landed order, if logged in full, could potentially be executed in a later block, causing losses for the order owner.
- Even if an order has built-in protections against unexpected executions, the order owner might still incur gas fees.

## External Error Redaction

While we don't log full orders ourselves, we sometimes interact with external systems and log their error codes. Since some of these may contain plain strings, we offer an option to redact any error before logging.

### Enabling Error Redaction

To enable external error redaction add the following flag to your configuration:
   ```
   redact_errors = true
   ```

### Example of Error Redaction

Consider this error enum:

```rust
pub enum SubmitBlockErr {
    ...
    RPCSerializationError(String),
    ...
}

```

the redaction match branch for this code is:

```rust
SubmitBlockErr::RPCSerializationError(plain_text) => SubmitBlockErr::RPCSerializationError(REDACTED.to_string())
```

Where
```rust
const REDACTED: &str = "[REDACTED]";
```
