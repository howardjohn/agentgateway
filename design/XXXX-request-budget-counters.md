# EP-XXXX: Request Budget Counters

- Issue: N/A
- Related: Request log storage and analytics
- Status: draft
- Date: 6/4/2026

> **Note:** This design reflects the proposal as of the date above. The current implementation may differ as the design
> is implemented, reviewed, or revised.

## Summary

Add request budget enforcement on top of persistent request logs using hot in-memory counters and durable SQL-backed
counter rows. Budget keys are arbitrary strings produced by user policy, such as a user ID, tenant ID, team ID, model
bucket, or a composition of those values.

The first version intentionally provides eventual multi-replica enforcement. Each replica enforces against its local
view, flushes usage deltas to the database, and can reconcile from request logs. Strict global reservation across
replicas is out of scope for the initial design.

## Goals

- Support budgets over arbitrary policy-generated keys.
- Support cost, token, and request-count budgets.
- Keep request-path checks fast by reading and updating in-memory counters.
- Persist counters to SQLite or Postgres.
- Recover counters after restart by loading durable state and optionally replaying request logs.
- Allow multi-replica deployments to temporarily exceed a budget and converge during flush/reconciliation.

## Non-Goals

- Strict cross-replica budget enforcement on every request.
- SQL materialized views for budget enforcement.
- A user-facing budget API in the first implementation.
- A complete pricing system. The budget layer consumes already-computed usage units.
- Implementing user policy key selection in this design; policy code supplies the key.

## Schema

Budget counters are mutable enforcement state. Request logs remain the audit source used for repair and analytics.

Use one table with the active window encoded in the primary key:

```sql
CREATE TABLE budget_counters (
  key TEXT NOT NULL,
  window_start TEXT NOT NULL,
  window_end TEXT NOT NULL,

  limit_units BIGINT NOT NULL,
  current_units BIGINT NOT NULL DEFAULT 0,
  unit TEXT NOT NULL,

  window_kind TEXT NOT NULL,
  window_duration TEXT NOT NULL,

  updated_at TEXT NOT NULL,
  flushed_at TEXT,

  PRIMARY KEY (key, window_start)
);
```

Postgres should use `TIMESTAMPTZ` for timestamps. SQLite should use RFC3339 `TEXT` timestamps.

`unit` values:

- `cost_micros`
- `tokens`
- `requests`

Cost should be stored as integer micro-units rather than floats. If we need finer precision later, use nanodollars or
provider-specific integer billing units.

`window_kind` values:

- `rolling`
- `calendar`

Keeping `window_start` in the primary key avoids reset races. If one replica is flushing usage for an old window while
another has moved to a new window, both writes land in distinct rows.

## Runtime Design

The runtime keeps counters in memory:

```text
BudgetKey -> BudgetCounter {
  key
  limit_units
  current_units
  unit
  window_start
  window_end
  dirty_delta
}
```

On request admission:

```text
derive policy key
load or create in-memory counter
reset local counter if window expired
reject if current_units >= limit_units
allow otherwise
```

On request completion:

```text
derive the same policy key
compute usage units
reset local counter if window expired
current_units += usage_units
dirty_delta += usage_units
```

This is post-paid accounting for token/cost budgets because final usage is normally known only after the upstream
response. A later strict mode can add reservations.

## Flush Design

Each replica periodically flushes additive deltas, not full snapshots:

```sql
UPDATE budget_counters
SET current_units = current_units + $delta,
    updated_at = $now,
    flushed_at = $now
WHERE key = $key
  AND window_start = $window_start
  AND window_end = $window_end;
```

If the row does not exist, insert it with `current_units = delta`.

The local process should only clear `dirty_delta` after a successful flush. Failed flushes leave the delta in memory for
the next flush attempt.

Replicas should periodically pull fresh DB counters and update local state. This reduces overshoot without adding
request-path SQL locks.

## Restart and Reconciliation

On startup:

1. Load active `budget_counters` rows into memory.
2. Create rows for configured policies that do not yet have an active window.
3. Reset or advance expired windows.
4. Optionally replay request logs for active windows and replace `current_units` with recomputed totals.

Replay requires request logs to contain:

- completion timestamp
- usage units, such as `total_tokens` or computed cost
- attributes JSON needed by the policy key function

Replay runs by scanning logs for the active window, deriving the budget key for each row, aggregating usage in memory,
and replacing the counter rows for that window.

## Multi-Replica Behavior

The initial model is eventually consistent:

- Each replica enforces from local memory.
- Replicas can exceed the global budget between flush and pull intervals.
- Flushes add local deltas into the shared DB row.
- Periodic pulls let replicas observe global usage and begin rejecting locally.
- Log replay can repair missed usage after crashes.

Overshoot can be reduced by:

- shorter flush intervals
- shorter DB pull intervals
- local headroom, such as rejecting at 95% of the limit
- request-count reservations for high-risk budgets

Strict global budgets would require a different mode, such as atomic SQL reservation, Redis counters, or another shared
coordination mechanism.

## Compatibility and Migration

Budget counters are opt-in and should not affect existing request logging behavior. The table can be created alongside
`request_logs` and `request_log_payloads`.

Existing logs can be replayed into counters only when the configured policy can derive keys from stored log attributes.
If prompt/completion payloads are not stored, replay still works for token/request budgets and any cost budget where
cost can be computed from typed log fields and attributes.

## Risks and Tradeoffs

- Eventual multi-replica enforcement can overspend.
- Post-paid accounting can allow one or more requests after the true limit is crossed.
- Startup replay may be expensive for large windows unless bounded, batched, or indexed.
- Arbitrary policy keys make indexing difficult; budget counters are indexed by resolved key, not by the source
  attribute.
- Cost computation must be deterministic if replay is expected to match online accounting.

## Test Plan

- Unit test window calculation for rolling and calendar windows.
- Unit test local counter reset, increment, and rejection behavior.
- SQLite and Postgres tests for additive flush and idempotent active-row creation.
- Test that failed flushes preserve dirty deltas.
- Test concurrent flushes from simulated replicas add rather than overwrite usage.
- Test startup load from existing rows.
- Test log replay recomputes and replaces active-window counters.
- Test old-window and new-window flushes do not race because of the `(key, window_start)` primary key.

## Open Questions

- What is the minimum flush interval we are comfortable hardcoding or configuring?
- Should DB pull be periodic only, or also triggered when local usage approaches the limit?
- Do we want an explicit strict mode with SQL reservations later?
- Should cost be stored as microdollars, nanodollars, or provider billing units?
- How much history should `budget_counters` retain after windows expire?
