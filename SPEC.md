# Route delegation

HTTPRoutes should be able to compose with other HTTPRoutes, rather than just a single route pointing to a backend.
So we can have Route --> Route --> Backend.

When we have multiple routes traversed, we will loop through until we get to a backend.
Cycles are detected at runtime and return errors.

We will essentially loop on select_best_route until we get to the end. RoutePath will contain a vec of routes traversed.
route_policies will fetch policies for each route along the path, with later routes taking precedence over earlier routes.
log.path_match will be the final path match. Note each route independently matches, so Child-Route must match a superset of Parent-Route to ever match.

In Route, we have `pub backends: Vec<RouteBackendReference>,`. We need to enhance RouteBackendReference  to support a BackendReference OR another route.

## Control plane

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: parent
  namespace: kgateway-system
spec:
  hostnames:
  - delegation.example
  parentRefs:
  - name: http
  rules:
  - matches:
    - path:
        type: PathPrefix
        value: /anything/team1
    backendRefs:
    - group: gateway.networking.k8s.io
      kind: HTTPRoute
      name: "*"
      namespace: team1
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: child-team1
  namespace: team1
spec:
  rules:
  - matches:
    - path:
        type: PathPrefix
        value: /anything/team1/foo
    backendRefs:
    - name: httpbin
      port: 8000
```
is the example a user will have

We will define a fwe new concepts.

A RouteGroup is a new primitive that the `team1/*` refers to. The parent route will have a backendRef with the RouteGroup there.
The child-team1 and other routes will have, instead of ListenerKey to bind them to a parent, a RouteGroupKey to bind them to the route group.