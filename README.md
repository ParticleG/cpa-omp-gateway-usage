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
| Linux ABI | Debian 12 / glibc; not musl or Alpine |
| Browser login | Same-origin CPAMC session with **Remember password** enabled |

The latest published release, `v0.1.1`, supports Linux x64 only. Five-platform validation for `v0.1.2` is in progress, but publication is blocked: the Go shared library crashes the CPA host on Intel macOS with `fatal error: unknown caller pc`. Do not load a `darwin/amd64` development build. The other four platforms passed native integration checks; no `v0.1.2` release has been published. See [the store submission requirements](https://github.com/ParticleG/cpa-omp-gateway-usage/issues/1). This is an independent community plugin, not an official Anthropic or OMP integration.

## Install

### 1. Install the native library

Download the ZIP for your OS and architecture and `checksums.txt` from [GitHub Releases](https://github.com/ParticleG/cpa-omp-gateway-usage/releases). Archives are named `omp-gateway-usage_<version>_<goos>_<goarch>.zip`.

Compare the downloaded ZIP's SHA-256 with its entry in `checksums.txt`. GNU users can run `sha256sum --ignore-missing -c checksums.txt`; macOS users can compute the hash with `shasum -a 256`, and Windows users with PowerShell `Get-FileHash -Algorithm SHA256`.

For available releases, extract the native library under `<plugins.dir>`. The intended layout for the five release targets is:

| Platform | Library path relative to `<plugins.dir>` |
| --- | --- |
| Linux x64 | `linux/amd64/omp-gateway-usage.so` |
| Linux ARM64 | `linux/arm64/omp-gateway-usage.so` |
| macOS Intel | `darwin/amd64/omp-gateway-usage.dylib` |
| macOS Apple Silicon | `darwin/arm64/omp-gateway-usage.dylib` |
| Windows x64 | `windows/amd64/omp-gateway-usage.dll` |

Each archive also includes the project license and third-party notices. Once this plugin is listed in the official CPA store, its store installer can perform the binary installation; private Gateway connectivity and token-file access still require operator configuration.

### 2. Provide private Gateway connectivity and token-file access

CPA must be able to reach the Gateway and read its main-token file. For Docker deployments, add both services to the same private network and merge this volume into the **existing** CPA service:

```yaml
services:
  cli-proxy-api:
    volumes:
      - /path/to/gateway/.omp/auth-gateway.token:/run/secrets/omp-gateway-usage.token:ro
```

Mount only the Gateway main-token file, not the Broker vault or an entire credentials directory. Recreate the CPA container after adding the mount. Keep the existing scoped token on the Claude inference provider unchanged.

For native CPA on macOS or Windows, provide a private token file readable by the CPA process and configure its absolute path below. The Gateway can remain on another privately reachable host; its credentials must not be exposed through the browser.

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

`gateway_url` must be an HTTP(S) origin without credentials, query or path. Private Docker-network HTTP is supported; use HTTPS for connections outside that private network. `token_file` must be an absolute server-side path. The defaults match the Linux Docker example above. On native Windows, configure a drive-qualified path such as `C:/omp-gateway/usage.token` before enabling the plugin; replace this example with the actual main-token file location. On native macOS, use the actual absolute path on that host.

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
node --check web/app.js
```

Release packaging requires Python 3.12+ and Go on `PATH`. Linux also requires Docker; macOS requires Xcode Command Line Tools, and Windows requires a MinGW-w64 GCC toolchain. The release workflow uses Go 1.26.

```sh
python scripts/package.py 0.1.2
python scripts/smoke.py dist/omp-gateway-usage_0.1.2_linux_amd64.zip 0.1.2
```

Use your Python 3 executable (`python3` if appropriate). The smoke command above is a Linux x64 example; pass the archive produced for your native platform.

The packager detects the host Go OS/architecture, runs Go race tests, stamps the supplied numeric version into plugin metadata, and writes the platform ZIP under `dist/`. Linux builds stay inside Debian 12 Docker to preserve the glibc baseline on both architectures. macOS and Windows build with their native C toolchains. If your build environment requires a different Go module proxy, set `GOPROXY` when invoking the packager.

The smoke check requires GitHub CLI (`gh`). It downloads and checksum-verifies the matching official CPA `8.0.43` release, extracts the plugin from the actual ZIP, and runs CPA natively with temporary configuration, synthetic quota data and temporary credentials. It checks registration, the embedded version, quota projection, management authentication and inference-key separation. It does not use a real Claude account or production configuration.

The release workflow builds and smoke-tests every supported platform on native GitHub-hosted runners. Only after all platform jobs succeed does it combine the ZIPs, generate one `checksums.txt`, and publish the release. A failed platform prevents a partial release.

Push a `v<major>.<minor>.<patch>` tag to release that tag, or manually run the workflow with a new numeric `version`. Manual runs create the tag at the verified source commit only after all platform jobs pass. The workflow never replaces existing tags or release assets.

## Upgrades and removal

Do not overwrite a loaded native library (`.so`, `.dylib` or `.dll`) in place. Use the CPA store's supported upgrade flow, or stop CPA before replacing the library and start it afterward. Preserve existing plugins, upstream providers and access policies.

To remove this integration, remove only its plugin configuration, library and token-file mount, then restart CPA. Recreate Docker containers when removing bind mounts. Do not delete the Gateway token or Broker OAuth credentials, which belong to the existing Gateway deployment.

## License and acknowledgements

Project code is released under the [MIT License](LICENSE). [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) preserves notices for CPA's native ABI examples, the Key Model Access management-session integration, the Go runtime and `gopkg.in/yaml.v3`. Those components retain their respective licenses.
