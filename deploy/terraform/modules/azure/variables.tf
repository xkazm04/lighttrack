variable "resource_group_name" {
  type        = string
  description = "Existing resource group to deploy into."
}

variable "location" {
  type    = string
  default = "eastus"
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
}

variable "database_url" {
  type      = string
  sensitive = true
  default   = ""
  # postgres://... (Phase 5a). Empty => ephemeral SQLite.
}

variable "allow_public" {
  type    = bool
  default = true
}

variable "min_replicas" {
  type    = number
  default = 0
}

variable "max_replicas" {
  type    = number
  default = 2
}
