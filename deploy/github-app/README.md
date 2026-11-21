# Central aq-git GitHub App rollout

This installs `github-app-aq-graph` for `https://github.com/ElectricJack/aq-graph.git` on `aq-ms1`. The host timer uses the owner-approved v1 GitHub App to request an installation token restricted to repository ID `1440134880` with `contents:write`. GitHub App installation tokens expire after one hour. The timer refreshes every ten minutes and atomically replaces the token. A failed refresh leaves the prior token file in place. GitHub rulesets and the App’s installation permissions still govern pushes and deletions. [GitHub App installation authentication](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/authenticating-as-a-github-app-installation)

## Install the refresher

Run these steps on the Linux host that supplies bind mounts to Docker. Use the existing App’s client ID, the ID of its installation with access to `ElectricJack/aq-graph`, and the App’s PEM key. The App installation must have repository Contents read/write. Provision the PEM through the owner’s existing secret handling at `/etc/aq-git/github-app.pem` with root ownership and mode `0600`. If the owner already has it at a root-readable managed path, override the unit’s `LoadCredential=` with that source path. The PEM stays on the host, outside the repository and the directory mounted into aq-git. The App client and installation IDs go in `/etc/aq-git/github-app.env`. [GitHub App JWT setup](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-a-json-web-token-jwt-for-a-github-app)

Run the following from the unpacked rollout bundle on the host.

```sh
sha256sum -c SHA256SUMS
sudo mkdir -p /usr/local/libexec /etc/aq-git
sudo install -d -m 0711 /var/lib/aq-git-github-app
sudo install -m 0755 refresh.py /usr/local/libexec/aq-git-github-app-refresh.py
sudo install -m 0755 aq-git-launcher /usr/local/libexec/aq-git-github-app-launcher
sudo install -m 0755 aq-git-bookworm /usr/local/libexec/aq-git-bookworm
sudo install -m 0644 compose.github-app.yaml /etc/aq-git/compose.github-app.yaml
sudo install -m 0644 aq-git-github-app.service aq-git-github-app.timer /etc/systemd/system/
sudoedit /etc/aq-git/github-app.env
```

Use the two variable names from `github-app.env.example` in the environment file. The external profile file takes precedence over the state-root profile file. The inspected `aq-ms1` central service advertised an empty label list. If that has changed, merge all existing profiles into the new file and keep their token and key paths accessible to aq-git before switching.

```sh
sudo install -m 0644 credential_profiles.json /var/lib/aq-git-github-app/credential_profiles.json
sudo systemctl daemon-reload
sudo systemctl start aq-git-github-app.service
sudo stat -c '%a %u:%g %n' /var/lib/aq-git-github-app/aq-graph.token
sudo journalctl -u aq-git-github-app.service -n 5 --no-pager
sudo systemctl enable --now aq-git-github-app.timer
```

Expect `600 10001:10001` and a successful refresh with an expiry timestamp. The unit needs host Python 3.11 or newer, OpenSSL, system trust roots, correct time and outbound HTTPS to `api.github.com`. Keep the host directory in place. Updating a file by rename within that directory is visible inside the container through the directory bind mount.

## Switch central aq-git

Drain and back up the service before replacing the central executable. Record the current `repo.show` binding for rollback. Append `-f /etc/aq-git/compose.github-app.yaml` to the existing `aq-ms1` Compose invocation, keeping its project name, original files, and interpolation environment. Check the merged `aq-git` config and recreate only that service with `up -d --no-deps --no-build --force-recreate aq-git`. [Drain and backup procedure](https://github.com/ElectricJack/aq-git/blob/main/docs/operations.md#local-release-manifests-and-upgrades)

The override keeps the existing supervisor and Git runtime. Its launcher adds `--credential-profiles /run/aq-git-github-app/credential_profiles.json` only to `serve`. It retains the live JSON socket `/run/aq-git/central.sock`. A smart HTTP socket also requires a coordinated server-side rollout and its own explicit `--git-http-socket` argument.

Check the central socket before changing the binding.

```sh
docker exec aq-ms1-aq-git-1 aq-git --socket /run/aq-git/central.sock --json capabilities
```

Expect profile configuration `available`, the label `github-app-aq-graph`, and operations `ref.show` and `ref.delete`. Apply a drained `repo.binding-update` through the owner’s normal binding workflow. Use the complete existing binding with its generation incremented by one, `remote_url` set to `https://github.com/ElectricJack/aq-graph.git`, and `credential_profile` set to `github-app-aq-graph`. The request’s `expected_binding_generation` is the prior generation. For published task branches, keep the project’s configured `allowed_target_prefixes`, such as `refs/heads/aq/`. A `binding_in_use` refusal identifies live resources that must be resolved before the switch. [Binding update API](https://github.com/ElectricJack/aq-git/blob/main/docs/api.md)

Verify authorized remote read access with `repo.sync`. For the write check, publish an approved disposable branch under the configured prefix with the absent-ref lease. Delete it with `ref.delete`, `remote:true`, and the exact published OID after any PR for the branch is closed. Repeat the check after a scheduled refresh while the same aq-git process is running. Keep the payload and operation ID for an indeterminate mutation. [Remote error and reconciliation guidance](https://github.com/ElectricJack/aq-git/blob/main/docs/api.md#remote-authentication-and-branch-creation)

To roll back, drain the project and restore the saved binding values through `repo.binding-update` at the next generation. Remove the extra Compose layer and recreate `aq-git` using the original invocation once the restored binding is compatible with the previous executable. Disable the timer after the binding stops using the App profile and remove the expired token file per the owner’s secret handling. Preserve state and outstanding operation evidence during rollback.

## Add another repository to the same installation

Give the existing App access to the repository in GitHub. From the checkout of the repository being enrolled, run the following to obtain its canonical name and numeric ID.

```sh
gh api 'repos/{owner}/{repo}' --jq '"AQ_GITHUB_REPOSITORY=" + .full_name + "\nAQ_GITHUB_REPOSITORY_ID=" + (.id|tostring)'
```

Save those two lines in `/etc/aq-git/repositories/PROJECT.env`, replacing `PROJECT` with a unique local name. Install the current refresh helper and the two `aq-git-github-app@.*` template units at the paths used above. Run `systemctl daemon-reload`, start `aq-git-github-app@PROJECT.service`, then enable `aq-git-github-app@PROJECT.timer` with `--now`. Each instance requests a token for one repository and verifies the name, ID and permissions returned by GitHub.

Mount `/var/lib/aq-git-github-app-PROJECT` read-only into central aq-git at a unique directory. Add a named `https_token` entry to its existing credential profile map. Set `remote_url` to the exact repository HTTPS URL including `.git`, and `token_path` to `repository.token` inside that instance's mounted directory. Reference the new profile from the repository binding. Verify the token is readable by aq-git's UID and run the publication and cleanup check above before enrolling AQ tasks. Each timer can be stopped and each repository disabled independently.
