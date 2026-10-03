terraform {
  required_version = ">= 1.5"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = ">= 5.0"
    }
    google = {
      source  = "hashicorp/google"
      version = ">= 5.0"
    }
  }
}

provider "aws" {
  alias  = "frankfurt"
  region = "eu-central-1"
}

# Bahrain is an opt-in region: enable it in the AWS account first.
provider "aws" {
  alias  = "bahrain"
  region = "me-south-1"
}

provider "google" {
  project = var.gcp_project
}
