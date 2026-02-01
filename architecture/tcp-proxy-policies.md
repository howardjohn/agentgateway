# TCP Proxy Mode Policy Compatibility

This document provides a comprehensive overview of which policies are applicable in TCP proxy mode versus HTTP proxy mode.

## Background

Agentgateway supports two primary proxy modes:
- **HTTP/HTTPS proxy mode**: Full Layer 7 (HTTP) protocol inspection and manipulation
- **TCP/TLS proxy mode**: Layer 4 (TCP) proxy with optional TLS termination or passthrough

Because TCP proxy mode operates at Layer 4, it has limited visibility into the application protocol. This means many HTTP-specific policies that require understanding or modifying HTTP headers, methods, paths, or bodies are not applicable.

## Policy Categories

Agentgateway organizes policies into three categories:

1. **Frontend Policies**: Applied at the listener level, controlling how connections are accepted
2. **Traffic Policies**: Applied at the route level, processing incoming requests
3. **Backend Policies**: Applied at the backend level, controlling outbound connections

## TCP Proxy Mode Policy Compatibility Table

### Frontend Policies

Frontend policies control listener configuration and are mostly protocol-agnostic.

| Policy | TCP Proxy Mode | HTTP Proxy Mode | Notes |
|--------|----------------|-----------------|-------|
| `HTTP` | ❌ No | ✅ Yes | HTTP/1.1 specific settings (timeouts, max headers, etc.) |
| `TLS` | ✅ Yes | ✅ Yes | TLS termination/passthrough configuration works for both |
| `TCP` | ✅ Yes | ✅ Yes | TCP keepalive settings apply to all TCP connections |
| `AccessLog` | ✅ Yes | ✅ Yes | Logging works for both modes (with different available fields) |
| `Tracing` | ⚠️ Limited | ✅ Yes | Basic connection tracing works, but HTTP-specific trace propagation does not |

### Traffic Policies

Traffic policies operate on requests and are predominantly HTTP-specific. Most are **not applicable** to TCP proxy mode.

| Policy | TCP Proxy Mode | HTTP Proxy Mode | Notes |
|--------|----------------|-----------------|-------|
| `Timeout` | ❌ No | ✅ Yes | HTTP request/response timeouts - requires HTTP protocol understanding |
| `Retry` | ❌ No | ✅ Yes | Retries are based on HTTP status codes |
| `AI` | ❌ No | ✅ Yes | LLM traffic inspection requires HTTP/JSON parsing |
| `Authorization` | ❌ No | ✅ Yes | Evaluates HTTP headers, paths, and methods |
| `LocalRateLimit` | ❌ No | ✅ Yes | Typically based on HTTP requests |
| `RemoteRateLimit` | ❌ No | ✅ Yes | External rate limiting for HTTP traffic |
| `ExtAuthz` | ❌ No | ✅ Yes | External authorization requires HTTP protocol |
| `ExtProc` | ❌ No | ✅ Yes | External processing of HTTP requests/responses |
| `JwtAuth` | ❌ No | ✅ Yes | JWT validation from HTTP headers |
| `BasicAuth` | ❌ No | ✅ Yes | HTTP Basic Authentication from headers |
| `APIKey` | ❌ No | ✅ Yes | API key extraction from HTTP headers |
| `Transformation` | ❌ No | ✅ Yes | CEL-based HTTP transformation |
| `Csrf` | ❌ No | ✅ Yes | CSRF protection for HTTP requests |
| `RequestHeaderModifier` | ❌ No | ✅ Yes | Modifies HTTP request headers |
| `ResponseHeaderModifier` | ❌ No | ✅ Yes | Modifies HTTP response headers |
| `RequestRedirect` | ❌ No | ✅ Yes | HTTP redirects with status codes |
| `UrlRewrite` | ❌ No | ✅ Yes | Rewrites HTTP URLs |
| `HostRewrite` | ❌ No | ✅ Yes | Rewrites HTTP Host header |
| `RequestMirror` | ❌ No | ✅ Yes | Mirrors HTTP requests |
| `DirectResponse` | ❌ No | ✅ Yes | Returns HTTP responses directly |
| `CORS` | ❌ No | ✅ Yes | Cross-Origin Resource Sharing for HTTP |

### Backend Policies

Backend policies control outbound connections to backends. Some are protocol-agnostic and work with TCP proxy mode.

| Policy | TCP Proxy Mode | HTTP Proxy Mode | Notes |
|--------|----------------|-----------------|-------|
| `McpAuthorization` | ❌ No | ✅ Yes | MCP protocol authorization - requires HTTP |
| `McpAuthentication` | ❌ No | ✅ Yes | MCP protocol authentication - requires HTTP |
| `A2a` | ❌ No | ✅ Yes | Agent-to-Agent protocol marking - requires HTTP |
| `HTTP` | ❌ No | ✅ Yes | HTTP/1.1 or HTTP/2 backend configuration |
| `TCP` | ✅ Yes | ✅ Yes | TCP backend configuration (keepalives, etc.) |
| `Tunnel` | ⚠️ Depends | ✅ Yes | CONNECT/HBONE tunneling - primarily for HTTP |
| `BackendTLS` | ✅ Yes | ✅ Yes | **TLS configuration for backend connections - works for TCP proxy** |
| `BackendAuth` | ✅ Yes | ✅ Yes | **Authentication to backend services - can work for TCP if backend supports it** |
| `InferenceRouting` | ❌ No | ✅ Yes | Routes to different AI inference backends based on HTTP content |
| `AI` | ❌ No | ✅ Yes | AI/LLM backend configuration - requires HTTP |
| `SessionPersistence` | ❌ No | ✅ Yes | HTTP session affinity based on cookies/headers |
| `RequestHeaderModifier` | ❌ No | ✅ Yes | Modifies HTTP request headers to backend |
| `ResponseHeaderModifier` | ❌ No | ✅ Yes | Modifies HTTP response headers from backend |
| `RequestRedirect` | ❌ No | ✅ Yes | HTTP redirects |
| `RequestMirror` | ❌ No | ✅ Yes | Mirrors HTTP requests to additional backends |

## Summary

### Policies Supported in TCP Proxy Mode

The following policies are supported in TCP proxy mode:

**Frontend Policies:**
- `TLS` - TLS termination/passthrough
- `TCP` - TCP keepalive configuration
- `AccessLog` - Connection logging
- `Tracing` - Basic connection tracing (limited)

**Backend Policies:**
- `TCP` - TCP backend configuration
- `BackendTLS` - TLS for backend connections
- `BackendAuth` - Authentication to backend (if backend protocol supports it)

### Key Takeaways

1. **TCP proxy mode is Layer 4**: It operates at the transport layer and cannot inspect or modify application-layer (HTTP) data.

2. **Limited policy set**: Only ~7 out of 30+ policies are applicable in TCP proxy mode.

3. **Primary use cases for TCP proxy**:
   - Proxying non-HTTP protocols (databases, message queues, custom protocols)
   - TLS termination/passthrough for any TCP protocol
   - Basic connection-level logging and metrics
   - Secure backend connections with mTLS

4. **Use HTTP proxy mode when possible**: If your traffic is HTTP-based, use HTTP/HTTPS listeners to take advantage of the full policy suite.

## Configuration Example

### TCP Proxy Configuration

```yaml
binds:
  - port: 5432  # PostgreSQL
    listeners:
      - name: postgres-proxy
        protocol: tcp  # TCP proxy mode
        routes:
          tcpRoutes:
            - name: postgres-route
              backends:
                - backend:
                    service:
                      name: postgres
                      namespace: default
                      port: 5432
                  inline_policies:
                    - backendTLS:  # Supported in TCP mode
                        mode: SIMPLE
                        caCertificates: /path/to/ca.crt

frontend_policies:
  - target:
      bind:
        port: 5432
    policies:
      - tcp:  # Supported in TCP mode
          keepalive:
            enabled: true
            time: 60s
      - tls:  # Supported in TCP mode
          cert: /path/to/cert.pem
          key: /path/to/key.pem
```

### HTTP Proxy Configuration (Full Policy Support)

```yaml
binds:
  - port: 8080
    listeners:
      - name: http-proxy
        protocol: http  # HTTP proxy mode - full policy support
        routes:
          - name: api-route
            matches:
              - path:
                  prefix: /api
            backends:
              - backend:
                  service:
                    name: api-service
                    namespace: default
                    port: 8080
            policies:
              - jwtAuth:  # Only available in HTTP mode
                  issuer: https://auth.example.com
              - cors:  # Only available in HTTP mode
                  allowOrigins:
                    - https://example.com
              - timeout:  # Only available in HTTP mode
                  request: 30s
```

## See Also

- [Configuration Architecture](configuration.md)
- [Configuration Schema](../schema/config.md)
- [Policy Constants (UI)](../ui/src/lib/policy-constants.ts)
