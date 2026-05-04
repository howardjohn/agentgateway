# WASM request policy

This example runs a Rust WebAssembly request policy.

Build the guest:

```sh
cargo build --manifest-path examples/wasm-request-policy/guest/Cargo.toml --target wasm32-unknown-unknown --release
```

Run the gateway with `config.yaml`, then send a request with `x-user: admin`.

The guest imports two host functions from `agentgateway:policy/host`:

- `http_get(url_ptr, url_len, out_ptr, out_cap) -> i32`
- `cel_eval_bool(expr_ptr, expr_len) -> i32`

`http_get` is backed by agentgateway's async `PolicyClient`. `cel_eval_bool`
evaluates against the current request using agentgateway's CEL executor.
