# Pricing: tiers, batch & flex

LightTrack computes per-event cost from a **DB-backed price book** (`model_prices`, seeded once from
`config/pricing.json`, then hot-swappable). Each row is `(provider, model, input_per_mtok,
output_per_mtok, cached_input_per_mtok?)`. Cost = `input·in_rate + output·out_rate + cached·cached_rate`
(cached tokens fall back to the input rate when no cached rate is set).

Beyond plain per-model rates, the book supports **prompt-length tiers** and **batch / flex** rates —
encoded as ordinary price rows with a modifier in the `model` name, so there is **no schema change**
and you manage them through the same `PUT /v1/prices/:provider/:model` endpoint.

## Variant rows

| Row `model`            | Meaning |
|------------------------|---------|
| `gemini-2.5-pro`       | base / standard rate |
| `gemini-2.5-pro@in>200000` | **prompt-length tier**: applies when input tokens exceed 200000 |
| `gpt-4o@batch`         | **batch** rate |
| `gpt-4o@flex`          | **flex** (priority) rate |

Resolution per call:
1. If the call's mode is `batch`/`flex` and a `@batch`/`@flex` row exists → use it; otherwise fall back
   to standard rates.
2. Standard lane: among `@in>N` rows, the **highest threshold the input exceeds** wins; if none, the
   base row.
3. Then the usual date-suffix fallback applies (e.g. `claude-haiku-4-5-20251001` → `claude-haiku-4-5`).

Tiers and mode variants compose only one level deep (a `@batch` row is a flat rate; it does not also
apply `@in>N` tiers). Define the variants you actually need.

## Setting rates

```bash
# base
curl -X PUT "$API/v1/prices/google/gemini-2.5-pro" -H 'authorization: Bearer <admin>' \
  -d '{"input_per_mtok":1.25,"output_per_mtok":10.0}'
# long-context tier (URL-encode @ and > → %40 %3E)
curl -X PUT "$API/v1/prices/google/gemini-2.5-pro%40in%3E200000" -H 'authorization: Bearer <admin>' \
  -d '{"input_per_mtok":2.5,"output_per_mtok":15.0}'
# batch rate
curl -X PUT "$API/v1/prices/openai/gpt-4o%40batch" -H 'authorization: Bearer <admin>' \
  -d '{"input_per_mtok":1.25,"output_per_mtok":5.0}'
```

(You can also seed any of these in `config/pricing.json` under `models`, keyed `"<provider>/<model>"`.)

## Telling LightTrack a call is batch / flex

The event carries the lane via either field (no new event column):

- `metadata.pricing_mode = "batch" | "flex" | "standard"` (explicit), **or**
- a tag: `"batch"`, or `"flex"` / `"priority"`.

Default is standard. Example ingest body:

```json
{ "provider": "openai", "model": "gpt-4o", "usage": { "input": 1000000, "output": 1000000 },
  "metadata": { "pricing_mode": "batch" } }
```

The client SDKs (`clients/`) pass these through their `metadata` / `tags` options.
