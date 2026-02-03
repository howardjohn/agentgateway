# Local Configuration Snapshot Tests

This directory contains snapshot tests for the local configuration handling in `local.rs`.

## Purpose

These tests verify that YAML configuration files can be successfully parsed into the `LocalConfig` structure. They use [insta](https://insta.rs/) for snapshot testing, which captures the configuration YAML and detects any unintended changes in how configurations are parsed.

## Test Files

- `*_config.yaml` - Input YAML configuration files copied from `examples/`
- `*_yaml.snap` - Snapshot files capturing the expected YAML structure after parsing

## Running Tests

```bash
# Run the tests
cargo test --package agentgateway --lib types::local::tests

# Review and accept snapshot changes (when making intentional changes)
cargo insta test --package agentgateway --lib --review
```

## Adding New Tests

To add a new test:

1. Copy a config file from `examples/` to this directory with the naming pattern `{test_name}_config.yaml`
2. Add a test function in `local.rs` that calls `test_config_parsing("{test_name}")`
3. Run tests with `INSTA_UPDATE=always` to generate the snapshot
4. Verify the snapshot is correct and commit it

## Test Coverage

Currently testing:
- **basic** - Basic MCP backend configuration with CORS policies
- **authorization** - JWT authentication and MCP authorization with multiple rules
- **multiplex** - Multiple MCP targets in a single backend

These examples cover the common configuration patterns and help catch bugs in YAML parsing and configuration deserialization.
