# Flatpak packaging

These files build Copperline as a Flatpak and are also what a Flathub
submission consists of.

| File | Purpose |
| --- | --- |
| `dev.copperline.Copperline.yaml` | Flatpak manifest (build steps, runtime, permissions) |
| `dev.copperline.Copperline.metainfo.xml` | AppStream metadata (store listing, screenshots, release notes) |
| `dev.copperline.Copperline.desktop` | Desktop-menu entry |
| `cargo-sources.json` | Vendored crate archives for the offline build (generated) |
| `generate-cargo-sources.sh` | Regenerates `cargo-sources.json` from `Cargo.lock` |

## Why cargo-sources.json exists

Flathub builds run with no network access, so Cargo cannot fetch crates from
crates.io. `cargo-sources.json` lists every dependency as a vendored source
plus a Cargo config that points Cargo at that offline registry. It is
generated from the committed `Cargo.lock`; regenerate it whenever the lockfile
changes:

```sh
./packaging/flatpak/generate-cargo-sources.sh
```

CI (`.github/workflows/flatpak.yml`) fails if the committed file is stale.

## Build and test locally (on Linux)

```sh
flatpak install flathub org.freedesktop.Platform//24.08 \
    org.freedesktop.Sdk//24.08 org.freedesktop.Sdk.Extension.rust-stable//24.08

flatpak run org.flatpak.Builder --force-clean --user --install \
    --install-deps-from=flathub --repo=repo builddir \
    packaging/flatpak/dev.copperline.Copperline.yaml

flatpak run dev.copperline.Copperline
```

The manifest's source is `type: dir path: ../..`, so it builds the checked-out
tree (this is also what CI validates). Build from a clean checkout: a `dir`
source copies the whole tree, including any large uncommitted ROM/disk images
sitting in the repo root.

## Lint before submitting

```sh
flatpak run --command=flatpak-builder-lint org.flatpak.Builder \
    manifest packaging/flatpak/dev.copperline.Copperline.yaml
flatpak run --command=flatpak-builder-lint org.flatpak.Builder repo repo
appstreamcli validate packaging/flatpak/dev.copperline.Copperline.metainfo.xml
```

## Submitting to Flathub

1. Make sure the manifest builds and lints clean, then switch the `type: dir`
   source to a `type: git` source pointing at the tagged release (`tag:` and
   `commit:`) you want published. The tag must already contain `assets/aros/`
   (the AROS ROM was added after `v0.1.0`), or the build's asset-install steps
   fail.
2. Fork <https://github.com/flathub/flathub>, branch off `new-pr`.
3. Add the manifest, metainfo, desktop file and `cargo-sources.json`.
4. Open a PR titled `Add dev.copperline.Copperline`.
5. Comment `bot, build` to trigger a test build; address reviewer feedback.
6. On merge, Flathub creates a `flathub/dev.copperline.Copperline` repo and
   grants write access (GitHub 2FA required). Future releases are published by
   bumping the `tag`/`commit` there.

## Known follow-up

The manifest grants `--filesystem=host` because Copperline's file dialogs
(rfd) do not yet use the XDG file-chooser portal. Migrating rfd to its
`xdg-portal` backend would let that permission be dropped, which Flathub
reviewers prefer. See the TODO in the manifest.
