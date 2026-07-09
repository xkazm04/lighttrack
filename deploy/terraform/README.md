# LightTrack — Terraform modules

Deploy the LightTrack API container to a cloud's serverless-container service with a common variable
interface, so the same inputs work across providers. Design: [`../../docs/PACKAGING.md`](../../docs/PACKAGING.md).

> ⚠️ **Status: authored, not yet applied.** These modules were written without a local `terraform`
> to validate. **Run `terraform init && terraform validate && terraform plan` against your own
> project/subscription before `apply`** — treat them as a reviewed starting point, not a guarantee.

## Modules
| Module | Service | Secrets | Status |
|---|---|---|---|
| `modules/gcp` | Cloud Run v2 | Secret Manager | template |
| `modules/azure` | Container Apps | Container Apps secrets | template |
| `modules/aws` | App Runner | Secrets Manager | TODO |

## Common interface (both modules)
| Variable | Meaning | Default |
|---|---|---|
| `image` | container image | `ghcr.io/xkazm04/lighttrack:v0.0.4` |
| `name` | resource name prefix | `lighttrack` |
| `auth_mode` | `enforced` / `dev` | `enforced` |
| `admin_key` | admin key (→ secret → `LIGHTTRACK_ADMIN_KEY`) | `""` |
| `database_url` | `postgres://…` (→ secret → `LIGHTTRACK_DATABASE_URL`); empty = SQLite | `""` |
| `allow_public` | public ingress | `true` |
| min/max instances (replicas) | autoscale bounds | 0 / 2 |

Outputs: `url` (+ `service_name`/`fqdn`).

## Bring-your-own database
These modules **don't provision a managed Postgres** (to avoid unverifiable private-networking config).
Pass a `database_url` from a managed/cloud-neutral Postgres — **Neon** or **Supabase** (free tiers) are the
easiest, and work from any cloud. Until Phase 5a lands the Postgres adapter, leaving `database_url` empty
runs ephemeral SQLite (fine for a smoke test; **not durable** on serverless — data resets on restart).

## Usage
```hcl
# main.tf
provider "google" { project = "my-proj" }          # or: provider "azurerm" { features {} }

module "lighttrack" {
  source       = "github.com/xkazm04/lighttrack//deploy/terraform/modules/gcp"
  project_id   = "my-proj"
  region       = "us-central1"
  admin_key    = var.admin_key            # set via TF_VAR_admin_key / a tfvars file, never commit
  database_url = var.database_url         # e.g. a Neon postgres:// URL
}

output "url" { value = module.lighttrack.url }
```
```bash
export TF_VAR_admin_key="$(openssl rand -hex 24)"
terraform init && terraform plan          # review, then: terraform apply
curl "$(terraform output -raw url)/health"
```

The `lt-runner` judge/queue worker runs **outside** the cloud service (it needs the `claude` CLI +
provider keys) — point it at the deployed URL: `lt-runner --base <url> --key <key> serve`.
