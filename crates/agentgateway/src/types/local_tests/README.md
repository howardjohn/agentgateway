# Local Configuration Snapshot Tests

This directory contains snapshot tests for the local configuration handling in `local.rs`.

## Purpose

These tests verify the full configuration normalization pipeline: YAML → `LocalConfig` → `NormalizedLocalConfig` → YAML. They use [insta](https://insta.rs/) for snapshot testing, capturing the normalized output to detect any unintended changes in configuration processing.

## Test Files

- `*_config.yaml` - Input YAML configuration files based on examples from `examples/`
- `*_normalized.snap` - Snapshot files capturing the expected normalized YAML output

## Running Tests

```bash
# Run the tests
cargo test --package agentgateway --lib types::local::tests

# Review and accept snapshot changes (when making intentional changes)
cargo insta test --package agentgateway --lib --review
```

## What Gets Tested

The tests validate:
1. **Parsing**: YAML → `LocalConfig` deserialization
2. **Normalization**: `LocalConfig` → `NormalizedLocalConfig` conversion
3. **Serialization**: `NormalizedLocalConfig` → YAML output

The snapshots capture the fully normalized configuration with all internal structures (keys, addresses, generated names, policies, backends), making it easy to detect regressions in any part of the pipeline.

## Adding New Tests

To add a new test:

1. Copy a config file from `examples/` to this directory with the naming pattern `{test_name}_config.yaml`
2. Add a test function in `local.rs` that calls `test_config_parsing("{test_name}").await`
3. Run tests with `INSTA_UPDATE=always` to generate the snapshot
4. Verify the snapshot is correct and commit it

## Test Coverage

Currently testing:
- **basic** - Basic MCP backend configuration with CORS policies
- **authorization** - MCP authorization rules with CEL expressions
- **multiplex** - Multiple MCP targets in a single backend

These examples cover common configuration patterns and help catch bugs in YAML parsing, configuration normalization, and serialization.
