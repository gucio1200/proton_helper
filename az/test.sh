#!/usr/bin/env bash

SUBSCRIPTION_ID="<your-subscription-id>"
LOCATION="westeurope"
API_VERSION="2020-11-01"

# Workload Identity env vars
CLIENT_ID="${AZURE_CLIENT_ID}"
TENANT_ID="${AZURE_TENANT_ID}"
TOKEN_FILE="${AZURE_FEDERATED_TOKEN_FILE}"

if [ ! -f "$TOKEN_FILE" ]; then
  echo "Federated token file not found: $TOKEN_FILE"
  exit 1
fi

FEDERATED_TOKEN=$(cat "$TOKEN_FILE")

SCOPE="https://management.azure.com/.default"

echo "[1] Requesting access token from Azure AD using Workload Identity..."

ACCESS_TOKEN=$(curl -s \
  -H "Content-Type: application/x-www-form-urlencoded" \
  -d "client_id=${CLIENT_ID}" \
  -d "scope=${SCOPE}" \
  -d "grant_type=urn:ietf:params:oauth:grant-type:token-exchange" \
  -d "requested_token_type=urn:ietf:params:oauth:token-type:access_token" \
  --data-urlencode "subject_token=${FEDERATED_TOKEN}" \
  --data-urlencode "subject_token_type=urn:ietf:params:oauth:token-type:jwt" \
  "https://login.microsoftonline.com/${TENANT_ID}/oauth2/v2.0/token" \
  | jq -r '.access_token'
)

if [ -z "$ACCESS_TOKEN" ] || [ "$ACCESS_TOKEN" == "null" ]; then
  echo "Failed to obtain ARM access token."
  exit 1
fi

echo "Access token obtained successfully."

URL="https://management.azure.com/subscriptions/${SUBSCRIPTION_ID}/providers/Microsoft.ContainerService/locations/${LOCATION}/orchestrators?api-version=${API_VERSION}"

echo
echo "[2] Calling AKS Orchestrators API:"
echo "URL: $URL"
echo

curl -s \
  -H "Authorization: Bearer ${ACCESS_TOKEN}" \
  -H "Content-Type: application/json" \
  "$URL" | jq .

