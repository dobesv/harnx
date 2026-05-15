# Private Package Registries and Repositories

harnx supports installing packages from private OCI registries and private Git repositories.

## OCI Private Registries

To authenticate with private OCI registries, create YAML configuration files in the `~/.config/harnx/package_repos/` directory. Use one YAML file per registry.

### Configuration Format

A registry configuration file defines how to authenticate with a specific registry URL prefix.

```yaml
url: "ghcr.io/myorg"  # Prefix matched against the registry URL
username:
  env: "GHCR_USERNAME"       # Read from environment variable
password:
  env: "GITHUB_TOKEN"        # Read from environment variable
```

### Credential Sources

Credentials (username and password) can be sourced in three ways:

*   **`env`**: Reads the value from an environment variable at runtime.
    ```yaml
    password: { env: "VAR_NAME" }
    ```
*   **`command`**: Runs a command and uses its `stdout` (trimmed) as the credential.
    ```yaml
    password: { command: "gh auth token" }
    ```
*   **`value`**: Uses an inline literal value. Avoid using this for secrets.
    ```yaml
    username: { value: "literal" }
    ```

### Prefix Matching

harnx uses prefix matching to find the correct credentials for a registry:
*   **Most-specific wins**: If you have configs for both `ghcr.io` and `ghcr.io/myorg`, the latter will be used for any package matching that prefix.
*   **Scheme stripping**: The `oci://` scheme is stripped from registry URLs before matching is performed.

### Examples

#### 1. GitHub Container Registry (GHCR)
Configure access using a GitHub Personal Access Token (PAT).

```yaml
url: "ghcr.io/myorg"
username:
  env: "GHCR_USERNAME"
password:
  env: "GITHUB_TOKEN"
```

#### 2. Docker Hub
Configure access using your Docker Hub credentials.

```yaml
url: "docker.io"
username:
  env: "DOCKER_USERNAME"
password:
  env: "DOCKER_PASSWORD"
```

#### 3. Amazon ECR
ECR requires a fresh token for each session. Use the `command` source to fetch it dynamically.

```yaml
url: "123456789012.dkr.ecr.us-east-1.amazonaws.com"
username:
  value: "AWS"
password:
  command: "aws ecr get-login-password --region us-east-1"
```

---

## Git Private Repositories

harnx uses the system `git` binary to manage git-based packages. No harnx-specific configuration is needed for private Git repositories.

When you run `harnx-pkg add` or `harnx-pkg update`, harnx will use whatever git credentials are configured in your environment.

### Common Authentication Methods

1.  **GitHub CLI (Recommended for GitHub)**: Run `gh auth login` to authenticate. Git will automatically use your CLI session.
2.  **SSH Keys**: Configure your `~/.ssh/config` as normal and ensure your keys are added to `ssh-agent`.
3.  **`~/.netrc`**: For HTTPS authentication, store your credentials in a `~/.netrc` file.
4.  **macOS Keychain**: On macOS, credentials can be managed automatically via the `osxkeychain` credential helper.
