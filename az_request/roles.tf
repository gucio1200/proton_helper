# --------------------------
# Variables
# --------------------------
variable "subscription_id" {}
variable "resource_group_name" {}
variable "aks_managed_identity_principal_id" {} # Workload identity SP or managed identity objectId

# --------------------------
# 1. Create custom role definition
# --------------------------
resource "azurerm_role_definition" "aks_orchestrators_reader" {
  name        = "AKS Orchestrators Reader"
  scope       = "/subscriptions/${var.subscription_id}"
  description = "Read available Kubernetes orchestrators for AKS"
  permissions {
    actions     = [
      "Microsoft.ContainerService/locations/orchestrators/read"
    ]
    not_actions = []
  }
  assignable_scopes = [
    "/subscriptions/${var.subscription_id}"
  ]
}

# --------------------------
# 2. Assign the custom role to the service account / workload identity
# --------------------------
resource "azurerm_role_assignment" "assign_aks_orchestrators_reader" {
  scope                = "/subscriptions/${var.subscription_id}"
  role_definition_id   = azurerm_role_definition.aks_orchestrators_reader.id
  principal_id         = var.aks_managed_identity_principal_id
}
