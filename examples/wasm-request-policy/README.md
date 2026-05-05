# WASM request policy

This example runs a Rust WebAssembly request policy.

Build the guest:

```sh
cargo build --manifest-path examples/wasm-request-policy/guest/Cargo.toml --target wasm32-unknown-unknown --release
wasm-tools component new \
  examples/wasm-request-policy/guest/target/wasm32-unknown-unknown/release/wasm_request_policy_guest.wasm \
  -o examples/wasm-request-policy/guest/target/wasm32-unknown-unknown/release/wasm_request_policy_guest.component.wasm
```

Run the gateway with `config.yaml`, then send a `GET /admin` request with
`x-user: admin`.

The policy `apply` function receives the incoming request as a high-level WIT
record, so guest authors can write normal async Rust without raw exported C
functions or manual guest-memory handling.

The guest also imports two async host functions from `agentgateway:policy/host`:

- `http-call(url: string) -> result<string, string>`
- `cel-eval-bool(expr: string) -> result<bool, string>`

`http-call` is backed by agentgateway's async `PolicyClient`. `cel-eval-bool`
evaluates against the current request using agentgateway's CEL executor.
