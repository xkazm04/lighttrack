variable "project_id" {
  type        = string
  description = "GCP project id."
}

variable "region" {
  type    = string
  default = "us-central1"
}

variable "name" {
  type    = string
  default = "lighttrack"
}

variable "image" {
  type    = string
  default = "ghcr.io/xkazm04/lighttrack:v0.0.4"
}

variable "auth_mode" {
  type    = string
  default = "enforced"
}

variable "admin_key" {
  type      = string
  sensitive = true
  default   = ""
  # Required for enforced mode. Stored in Secret Manager and injected as LIGHTTRACK_ADMIN_KEY.
}

variable "database_url" {
  type      = string
  sensitive = true
  default   = ""
  # postgres://... (Phase 5a). Empty => the app uses ephemeral SQLite (lost on revision restart).
}

variable "allow_public" {
  type    = bool
  default = true
}

variable "min_instances" {
  type    = number
  default = 0
}

variable "max_instances" {
  type    = number
  default = 2
}
