# How a release is built

This page is for maintainers. It says what the release workflow does, which secrets and variables switch its optional steps on, and what you get without them.

A release starts with a tag `v<version>`. The tag must equal the workspace version in `Cargo.toml`, and the package metadata that carries the version as text (`packaging/linux/org.recursiveai.bytescape.metainfo.xml` and `packaging/linux/changelog`) must name it too. The workflow checks all three first and stops if they differ.

The workflow then builds every file on its own runner and puts them, with `SHA256SUMS`, in a **draft** GitHub Release. Publishing the draft is a decision for a person. A manual run of the workflow on a branch does the same builds and checks and keeps the files as workflow artifacts, without making a release. A manual run on a branch never signs and never uploads anything to a store.

## Where the credentials are

The jobs that compile code have no secret and no login token. Compiling runs our code and every dependency's build scripts, so nothing it could leak is within reach. Signing and the store upload are separate small jobs that only download finished files.

Those jobs run in a GitHub environment named `release`. The signing and store secrets are *environment* secrets of `release`, readable only by jobs that name it, and the environment accepts only tags that match `v*`. Only maintainers can create `v*` tags (a tag rule on the repository allows creating, changing or deleting them only to administrators), so a branch, a pull request or an ordinary contributor's push cannot run an edited workflow and read the secrets. The steps that use them are switched on by repository variables, which hold no secret:

| Variable | Turns on |
|---|---|
| `WINDOWS_SIGNING_ENABLED` | Signing the Windows programs and installers (tag builds only). |
| `LINUX_SIGNING_ENABLED` | Signing the `.deb` and the AppImage (tag builds only). |
| `SNAP_UPLOAD_ENABLED` | Uploading the strict snap to the store's `edge` channel (tag builds only). |
| `ARM64_CI` | Building the Linux ARM64 packages and running the ARM64 install checks on native ARM64 runners. Set it to `true` once the repository is public. |

Each optional step is skipped, with a warning, when its variable is not `true`, or when it is `true` but the secrets are missing. Nothing is signed with a stand-in, and nothing waits for a credential.

## What each job does

| Job | Runner | Result |
|---|---|---|
| `windows build` (x64, arm64) | `windows-2025` | The two programs and a debug-symbols zip, with no credential. ARM64 is cross-built here. |
| `windows sign programs` | `windows-2025`, `release` environment | Signs the two programs. Only when switched on. |
| `windows package` | `windows-2025` | The portable zip and the installer, made from the (signed) programs. |
| `windows sign installer` | `windows-2025`, `release` environment | Signs the installer. Only when switched on. |
| `windows check` | `windows-2025` | The x64 installer is installed, checked (files, permissions, version, right-click entries) and uninstalled. |
| `windows arm64 install check` | `windows-11-arm` | The same check for the ARM64 installer, when `ARM64_CI` is `true`. |
| `linux build` | `ubuntu-22.04`, and `ubuntu-22.04-arm` with `ARM64_CI` | The `.deb` and the AppImage, with no credential. |
| `linux sign` | the same, `release` environment | A signature file for each `.deb`, and the AppImage packed again with its embedded signature. Compiles nothing. Only when switched on. |
| `linux check` | the same | The final `.deb` is installed in clean Ubuntu 22.04 and 24.04 containers; the final AppImage is run on 22.04. |
| `arm64-compile-check` | `ubuntu-22.04` | Without a native ARM64 runner, checks that the ARM64 Linux code compiles. |
| `snap` | `ubuntu-24.04`, and the ARM64 runner with `ARM64_CI` | The strict snap and the classic-ready snap. The strict one is installed from the file and its plugs are checked. |
| `snap upload` | `ubuntu-24.04`, `release` environment | Uploads the strict snap to `edge`. Tag builds only, and only when switched on. Moving it to another channel is manual. |
| `assemble` | `ubuntu-24.04` | `SHA256SUMS`, the winget and Chocolatey files, and a build-provenance statement (public repositories only). |
| `chocolatey pack` | `windows-2025` | `choco pack` on the generated Chocolatey package. |
| `draft release` | `ubuntu-24.04` | The draft release, on a tag push only. |
| `oidc subject check` | `ubuntu-24.04`, `release` environment | Only when started by hand with `check_oidc_subject`: prints the subject the Azure login would present, so the federated credential can be matched to it. Builds nothing and uses no secret. |

## Secrets

Names only; the values are never in the repository. All are environment secrets of `release`.

| Name | Used for |
|---|---|
| `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, `AZURE_SUBSCRIPTION_ID` | Signing in to Azure by OpenID Connect (no stored client secret). The federated credential must name the subject `repo:recursive-ai-oss@<owner id>/bytescape@<repository id>:environment:release`: this repository uses GitHub's immutable subject form, which carries the numeric owner and repository ids. Run the `oidc subject check` job to see the exact text. |
| `AZURE_SIGNING_ENDPOINT`, `AZURE_SIGNING_ACCOUNT`, `AZURE_SIGNING_PROFILE` | Which signing account and certificate profile to use. Windows signing needs all six Azure secrets. |
| `LINUX_GPG_PRIVATE_KEY`, `LINUX_GPG_KEY_ID` | A detached signature next to each `.deb`, and the AppImage's embedded signature. |
| `SNAPCRAFT_STORE_CREDENTIALS` | Uploading the strict snap to the `edge` channel. |

## Building a file by hand

The workflow only calls scripts you can run yourself:

- `packaging/windows/build-windows.sh` (`stage`, `zip`, `install-inno`, `installer`)
- `packaging/linux/build-deb.sh`, `packaging/linux/build-appimage.sh`
- `packaging/snap/build-snap.sh strict|classic`
- `cargo xtask manifests --version V --sums SHA256SUMS` for the winget and Chocolatey files
- `packaging/local/wine-tests.sh` runs the Windows test programs under wine on Linux. That is evidence, not a replacement for a Windows run.
