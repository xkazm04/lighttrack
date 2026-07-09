<#
.SYNOPSIS
  LightTrack -> Google Cloud Run, one command (Windows / PowerShell mirror of deploy.sh).

.DESCRIPTION
  Deploys the API as a scale-to-zero Cloud Run service. Free-tier friendly: with -MinInstances 0
  you pay nothing while idle, and Cloud Run's always-free quota covers low-traffic test apps.

  Storage:
    - No -DatabaseUrl  => ephemeral SQLite (data LOST on cold start). Smoke-test only.
    - -DatabaseUrl ... => durable Postgres (e.g. a free Neon DSN). Recommended for real data.

  Auth defaults to enforced + a generated admin key (printed once). Ingress is public so apps can
  POST events with their project API keys; /health stays open; /v1 management needs the admin key.

.EXAMPLE
  ./deploy.ps1 -Project my-gcp-project
  ./deploy.ps1 -Project my-gcp-project -DatabaseUrl "postgres://user:pass@host/db?sslmode=require"
  ./deploy.ps1 -Project my-gcp-project -Build      # build the image from local source
#>
[CmdletBinding()]
param(
  [string]$Project,
  [string]$Region = "us-central1",
  [string]$Service = "lighttrack",
  [string]$Image = "ghcr.io/xkazm04/lighttrack:v0.0.4",
  [switch]$Build,
  [string]$DatabaseUrl = "",
  [string]$AdminKey = "",
  [string]$AuthMode = "enforced",
  [switch]$Private,
  [int]$MinInstances = 0,
  [int]$MaxInstances = 2,
  [string]$Cpu = "1",
  [string]$Memory = "256Mi"
)
$ErrorActionPreference = "Stop"
$root = (Resolve-Path "$PSScriptRoot/../..").Path

if (-not $Project) { $Project = (gcloud config get-value project 2>$null) }
if (-not $Project -or $Project -eq "(unset)") { throw "ERROR: -Project is required (or run 'gcloud config set project')." }
Write-Host ">> project=$Project region=$Region service=$Service auth=$AuthMode"

# --- 1. APIs ----------------------------------------------------------------
Write-Host ">> enabling APIs..."
# Artifact Registry is always needed: Cloud Run can only pull from Artifact Registry / gcr.io,
# so even a prebuilt public image is mirrored into the project's registry first.
$apis = @("run.googleapis.com","secretmanager.googleapis.com","artifactregistry.googleapis.com")
if ($Build) { $apis += "cloudbuild.googleapis.com" }
gcloud services enable @apis --project $Project --quiet

function Confirm-Repo {
  gcloud artifacts repositories describe $Service --location $Region --project $Project *> $null
  if ($LASTEXITCODE -ne 0) {
    gcloud artifacts repositories create $Service --repository-format=docker --location $Region --project $Project --quiet
  }
}

# --- 2. resolve a deployable image (Artifact Registry) ----------------------
if ($Build) {
  Write-Host ">> building image from source via Cloud Build (this takes a while)..."
  Confirm-Repo
  $Image = "$Region-docker.pkg.dev/$Project/$Service/${Service}:latest"
  Push-Location $root
  try { gcloud builds submit --project $Project --config deploy/cloudrun/cloudbuild.yaml --substitutions=_IMAGE=$Image }
  finally { Pop-Location }
  if ($LASTEXITCODE -ne 0) { throw "Cloud Build failed (exit $LASTEXITCODE)." }
}
elseif ($Image -notmatch '(-docker\.pkg\.dev/|(^|\.)gcr\.io/)') {
  # Cloud Run can't pull from external registries (ghcr.io/docker.io). Front the upstream with an
  # Artifact Registry *remote repository* (a lazy pull-through cache) and deploy that AR path.
  $regHost = ($Image -split '/',2)[0]
  $upstreamPath = ($Image -split '/',2)[1]
  $remoteRepo = "$Service-remote"
  gcloud artifacts repositories describe $remoteRepo --location $Region --project $Project *> $null
  if ($LASTEXITCODE -ne 0) {
    gcloud artifacts repositories create $remoteRepo --repository-format=docker --mode=remote-repository --remote-docker-repo="https://$regHost" --location $Region --project $Project --quiet
  }
  Write-Host ">> fronting $regHost with Artifact Registry remote repo '$remoteRepo'"
  $Image = "$Region-docker.pkg.dev/$Project/$remoteRepo/$upstreamPath"
}
Write-Host ">> image=$Image"

$projectNumber = (gcloud projects describe $Project --format='value(projectNumber)')
$runtimeSa = "$projectNumber-compute@developer.gserviceaccount.com"

# --- 3. secrets -------------------------------------------------------------
function Set-LtSecret([string]$Name, [string]$Value) {
  gcloud secrets describe $Name --project $Project *> $null
  if ($LASTEXITCODE -ne 0) { gcloud secrets create $Name --replication-policy=automatic --project $Project --quiet }
  # Write the value via a temp file (NOT a pipe): PowerShell appends a trailing newline to piped
  # stdin, which would corrupt the secret (e.g. the admin key wouldn't match what we print).
  $tmp = [System.IO.Path]::GetTempFileName()
  try {
    [System.IO.File]::WriteAllText($tmp, $Value, (New-Object System.Text.UTF8Encoding($false)))
    gcloud secrets versions add $Name --data-file=$tmp --project $Project --quiet | Out-Null
  } finally { Remove-Item $tmp -Force -ErrorAction SilentlyContinue }
  gcloud secrets add-iam-policy-binding $Name --project $Project --member="serviceAccount:$runtimeSa" --role="roles/secretmanager.secretAccessor" --quiet | Out-Null
}

$setSecrets = @()
$generatedKey = $false
if ($AuthMode -eq "enforced") {
  if (-not $AdminKey) {
    $bytes = New-Object byte[] 32
    [System.Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
    $AdminKey = ($bytes | ForEach-Object { $_.ToString('x2') }) -join ''
    $generatedKey = $true
  }
  Write-Host ">> storing admin key in Secret Manager ($Service-admin-key)"
  Set-LtSecret "$Service-admin-key" $AdminKey
  $setSecrets += "LIGHTTRACK_ADMIN_KEY=$Service-admin-key:latest"
}
if ($DatabaseUrl) {
  Write-Host ">> storing database URL in Secret Manager ($Service-database-url)"
  Set-LtSecret "$Service-database-url" $DatabaseUrl
  $setSecrets += "LIGHTTRACK_DATABASE_URL=$Service-database-url:latest"
} else {
  Write-Host "!! no -DatabaseUrl: using EPHEMERAL SQLite (data lost on cold start). Pass a Neon DSN for durable storage."
}

# --- 4. deploy --------------------------------------------------------------
Write-Host ">> deploying to Cloud Run..."
$deployArgs = @(
  "run","deploy",$Service,
  "--project",$Project,"--region",$Region,"--image",$Image,
  "--port","8080",
  "--set-env-vars","LIGHTTRACK_BIND=0.0.0.0:8080,LIGHTTRACK_AUTH_MODE=$AuthMode",
  "--cpu",$Cpu,"--memory",$Memory,
  "--min-instances",$MinInstances,"--max-instances",$MaxInstances,
  "--quiet"
)
if ($setSecrets.Count -gt 0) { $deployArgs += @("--set-secrets", ($setSecrets -join ",")) }
if ($Private) { $deployArgs += "--no-allow-unauthenticated" } else { $deployArgs += "--allow-unauthenticated" }
gcloud @deployArgs
if ($LASTEXITCODE -ne 0) { throw "Cloud Run deploy failed (exit $LASTEXITCODE)." }

$url = (gcloud run services describe $Service --project $Project --region $Region --format='value(status.url)')

# --- 5. health check --------------------------------------------------------
Write-Host ">> health check: $url/health"
try { $health = (Invoke-RestMethod -Uri "$url/health" -TimeoutSec 30) } catch { $health = "UNREACHABLE ($($_.Exception.Message))" }

Write-Host ""
Write-Host "============================================================"
Write-Host " LightTrack deployed: $url   (health: $health)"
if ($generatedKey) { Write-Host " ADMIN KEY (save now, shown once): $AdminKey" }
Write-Host "============================================================"
Write-Host " Next: create a project + ingest key, then point your apps at $url"
Write-Host "   POST $url/v1/projects  (Authorization: Bearer <ADMIN_KEY>)  {""name"":""demo""}"
