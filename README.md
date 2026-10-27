# OMP Gateway Usage

A read-only native [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) plugin that displays Claude subscription quota from a private [OMP Auth Gateway](https://github.com/can1357/oh-my-pi/blob/main/docs/auth-broker-gateway.md).

It fills a specific integration gap: CPA holds a scoped Gateway inference token, while Claude OAuth credentials remain in OMP Auth Broker. The dashboard reads Gateway usage without copying OAuth credentials back into CPA or changing inference routing.

## Features and scope

- Claude five-hour, weekly and any reported model-specific percentage windows.
- Used and remaining percentages, reset timestamps, and original upstream fetch times.
- Missing quota stays unavailable instead of being presented as zero usage.
- Manual refresh using the existing Gateway/Broker cache; no database or background poller.
- CPA management authentication, with a dashboard embedded in Management Center.
- Sanitized responses: no account IDs, email addresses, organization metadata, tokens or non-Claude reports.

Quota is account-wide and includes usage outside CPA. It is not a per-key allowance, a token-cost estimate, or a guarantee of future model availability. Monetary extra-usage limits are not shown. The plugin does not provide OAuth login, bypass provider policies, or claim to reduce account suspension risk.

## Compatibility

| Component | Supported or tested configuration |
| --- | --- |
| CLIProxyAPI | Tested with `8.0.43`; requires RPC schema 6+ and a CGO-enabled build |
| OMP Gateway | Tested with `18.12.6` and its `GET /v1/usage` report format |
| Release binary | Linux amd64, built on Debian 12; validated in the official CPA Debian-based container |
| Browser login | Same-origin CPAMC session with **Remember password** enabled |

The published Linux binary is not a musl/Alpine build. Windows, macOS and other architectures are not currently published. This is an independent community plugin, not an official Anthropic or OMP integration.

## Install

### 1. Install the native library

Download the Linux amd64 ZIP and `checksums.txt` from [GitHub Releases](https://github.com/ParticleG/cpa-omp-gateway-usage/releases). Verify the archive with `sha256sum -c checksums.txt`, then extract it and place `omp-gateway-usage.so` at:

```text
<plugins.dir>/linux/amd64/omp-gateway-usage.so
```

Each archive also includes the project license and third-party notices. Once this plugin is listed in the official CPA store, its store installer can perform the binary installation; the private network and token mount below still require operator configuration.

### 2. Share a private network and mount the token

CPA must be able to reach the Gateway. In Docker Compose, add both services to the same private network and merge this volume into the **existing** CPA service:

```yaml
services:
  cli-proxy-api:
    volumes:
      - /path/to/gateway/.omp/auth-gateway.token:/run/secrets/omp-gateway-usage.token:ro
```

Mount only the Gateway main-token file, not the Broker vault or an entire credentials directory. Recreate the CPA container after adding the mount. Keep the existing scoped token on the Claude inference provider unchanged.

### 3. Enable the plugin

Merge this into the existing CPA configuration without removing other plugins:

```yaml
plugins:
  enabled: true
  configs:
    omp-gateway-usage:
      enabled: true
      gateway_url: http://omp-gateway:4000
      token_file: /run/secrets/omp-gateway-usage.token
```

`gateway_url` must be an HTTP(S) origin without credentials, query or path. Private Docker-network HTTP is supported; use HTTPS for connections outside that private network. `token_file` must be an absolute server-side path. The defaults match the example above.

### 4. Open the dashboard

Sign in to CPA Management Center with **Remember password** enabled, then choose **OMP Gateway Usage** in the plugin menu. The standalone page is also available at:

```text
/v0/resource/plugins/omp-gateway-usage/status
```

Click **Refresh** to read the latest Gateway snapshot. This does not bypass the upstream cache, which normally refreshes about every five minutes. The page shows the original fetch time so cached data is not mistaken for a new upstream reading.

## Authentication and security boundary

The data endpoint is protected by CPA's own management middleware:

```text
GET /v0/management/plugins/omp-gateway-usage/usage
Authorization: Bearer <CPA management key>
```

- The resource page and JS/CSS are public static files containing no quota or credentials. Only the exact management endpoint can contact Gateway.
- Ordinary CPA inference API keys cannot read this endpoint, even when they can use Claude models.
- OMP denies `/v1/usage` to scoped client tokens. The plugin therefore reads the Gateway **main token** from the private file on each authenticated query. This token has broader privileges than a read-only quota token; it is never sent to the browser or returned by the plugin.
- The plugin does not follow upstream HTTP redirects or use environment-selected HTTP proxies, preventing the main token from being sent to those destinations.
- CPA native plugins are trusted in-process code. The token mount does not isolate this plugin from other native plugins or CPA administrators.
- The UI reuses the existing same-origin CPAMC `cli-proxy-auth` session, using its secureStorage v1 format and legacy key fallback. That format is reversible obfuscation, not encryption. The plugin does not persist another copy of the management key. Logout storage events clear displayed quota.
- Transport failures and unexpected responses produce sanitized errors, not zero-usage fallbacks.

Do not expose Gateway port 4000 publicly to make this dashboard work. If a token file is replaced with a new inode, recreate the CPA container so its file bind mount follows the replacement.

## Build and validate

Local Go development requires Go 1.24+ and a C compiler:

```sh
go test -race ./...
CGO_ENABLED=1 go build -trimpath -buildmode=c-shared \
  -ldflags '-X main.pluginVersion=dev' -o dist/omp-gateway-usage.so .
node --check web/app.js
```

Build a release package using Docker, Bash, `zip`, and `sha256sum`:

```sh
bash scripts/package.sh 0.1.1
python3 scripts/smoke.py dist/omp-gateway-usage.so 0.1.1
```

If your build environment requires a different Go module proxy, set `GOPROXY` when invoking the packaging script. Otherwise the build uses Go's default public proxy and direct fallback.

The packaging script builds for `linux/amd64` in Debian 12, runs the Go race tests, stamps the supplied numeric version into plugin metadata, and creates:

```text
dist/omp-gateway-usage_0.1.1_linux_amd64.zip
dist/checksums.txt
```

The smoke check runs the actual native plugin in an isolated local CPA container with synthetic quota data and temporary credentials. It never uses a real Claude account or production configuration. It requires Linux Docker host networking and Python 3.

The tag-triggered GitHub Actions workflow runs the same package and smoke commands before publishing a release. Tags use `v<major>.<minor>.<patch>`; the tag is the source of the embedded release version and archive filename. The workflow does not replace existing release assets.

## Upgrades and removal

Do not overwrite a loaded `.so` in place. Use the CPA store's supported upgrade flow, or stop CPA before replacing the library and start it afterward. Preserve existing plugins, upstream providers and access policies.

To remove this integration, remove only its plugin configuration, library and token-file mount, then recreate CPA. Do not delete the Gateway token or Broker OAuth credentials, which belong to the existing Gateway deployment.

## License and acknowledgements

Project code is released under the [MIT License](LICENSE). [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) preserves notices for CPA's native ABI examples, the Key Model Access management-session integration, the Go runtime and `gopkg.in/yaml.v3`. Those components retain their respective licenses.
