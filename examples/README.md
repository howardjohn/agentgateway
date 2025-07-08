## Examples

This directory contains examples of how to use Agentgateway. Each example covers a slightly different use-case, if you are unsure where to start, basic is the way to go. The examples increase in complexity, so we recommend following them in order.

### [Basic](basic/README.md)

The basic example is the simplest way to get started with Agentgateway.
This example exposes a single MCP server over Stdio.

### [Multiplex](multiplex/README.md)

The multiplex example shows how to use Agentgateway to multiplex multiple targets on a single listener.

### [Authorization](authorization/README.md)

The authorization example shows how to use Agentgateway to apply authorization policies to incoming requests. It uses JWT Authentication and authorizaton policies to authenticate and authorize incoming requests.

### [TLS](tls/README.md)

The tls example shows how to use Agentgateway to terminate TLS connections.

### [OpenAPI](openapi/README.md)

The openapi example shows how to use Agentgateway to serve an OpenAPI specification for a given target.

### [A2A](a2a/README.md)

The `a2a` example shows how to use Agentgateway to serve an [A2A](https://a2aproject.github.io/A2A/) agent.
