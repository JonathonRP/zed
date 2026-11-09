#!/usr/bin/env bash

# Source only from trusted control steps so the key and cleanup share their lifetime.
test "$GITHUB_REPOSITORY" = "JonathonRP/zed"
if [[ -z "${RP_STABLE_SYNC_DEPLOY_KEY:-}" ]]; then
  echo "::error::RP_STABLE_SYNC_DEPLOY_KEY is required. An owner must approve and configure a dedicated write deploy key for JonathonRP/zed before stable sync can push." >&2
  exit 1
fi

rp_stable_ssh_directory="$(mktemp -d "${RUNNER_TEMP}/rp-stable-sync-ssh.XXXXXX")"
trap 'rm -f -- "$rp_stable_ssh_directory/key" "$rp_stable_ssh_directory/known_hosts"; rmdir -- "$rp_stable_ssh_directory"' EXIT
printf '%s\n' "$RP_STABLE_SYNC_DEPLOY_KEY" > "$rp_stable_ssh_directory/key"
chmod 600 "$rp_stable_ssh_directory/key"
unset RP_STABLE_SYNC_DEPLOY_KEY

gh api meta --jq '.ssh_keys[] | "github.com " + .' \
  > "$rp_stable_ssh_directory/known_hosts"
if [[ ! -s "$rp_stable_ssh_directory/known_hosts" ]]; then
  echo "::error::GitHub returned no SSH host keys; refusing stable sync without verified host keys." >&2
  exit 1
fi

export GIT_SSH_COMMAND="ssh -i \"$rp_stable_ssh_directory/key\" -o IdentitiesOnly=yes -o UserKnownHostsFile=\"$rp_stable_ssh_directory/known_hosts\" -o StrictHostKeyChecking=yes"
