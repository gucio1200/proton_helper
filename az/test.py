import os
import requests
import json
from azure.identity import WorkloadIdentityCredential

# ---- CONFIG ----
subscription_id = "<your-subscription-id>"
location = "westeurope"
api_version = "2020-11-01"

# ---- Create Workload Identity Credential ----
credential = WorkloadIdentityCredential(
    tenant_id=os.environ["AZURE_TENANT_ID"],
    client_id=os.environ["AZURE_CLIENT_ID"],
    token_file_path=os.environ["AZURE_FEDERATED_TOKEN_FILE"],
)

# ---- Get ARM access token ----
token = credential.get_token("https://management.azure.com/.default")
access_token = token.token

# ---- Call orchestrators API ----
url = (
    f"https://management.azure.com/subscriptions/{subscription_id}"
    f"/providers/Microsoft.ContainerService/locations/{location}/orchestrators"
    f"?api-version={api_version}"
)

headers = {"Authorization": f"Bearer {access_token}"}

resp = requests.get(url, headers=headers)
resp.raise_for_status()

data = resp.json()

# ---- Pretty-print output ----
print(json.dumps(data, indent=2))
