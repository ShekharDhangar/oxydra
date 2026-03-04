### Build/install test script

Use `scripts/test-build-install.sh` to test three build sources on local Mac + SSH targets (like Raspberry Pi):

- `--source tag` → installs from GitHub release artifacts (via `install-release.sh`)
- `--source local` → builds current checkout and installs those binaries
- `--source commit --commit <rev>` → builds a temporary worktree checkout for that commit

Modes:

- `--mode fresh` creates an isolated install at `/tmp/oxydra-fresh-tests/<label>`
- `--mode upgrade` updates the existing install on each target
- `--mode fresh-clean --label <label>` removes the isolated fresh install

Environment handling:

- If `scripts/.env` exists (gitignored), values are loaded
- Fresh mode writes those values to `<fresh>/runner.env`
- Fresh mode creates `<fresh>/runner-with-env.sh` so any runner subcommand uses the same env + config automatically
- Upgrade mode also writes `<base-dir>/.oxydra/runner-with-env.sh` (and `runner.env.test-build` when env vars are loaded)

Examples:

```bash
# release tag
./scripts/test-build-install.sh --mode fresh --source tag --tag v0.2.3 \
  --target local --target ssh:pi@raspberrypi.local

# current checkout
./scripts/test-build-install.sh --mode fresh --source local \
  --target local --target ssh:pi@raspberrypi.local

# specific commit
./scripts/test-build-install.sh --mode upgrade --source commit --commit a1b2c3d \
  --target local --target ssh:pi@raspberrypi.local

# run any runner command with the same env/config from fresh install
/tmp/oxydra-fresh-tests/<label>/runner-with-env.sh --user alice start
/tmp/oxydra-fresh-tests/<label>/runner-with-env.sh --user alice logs --tail 200
/tmp/oxydra-fresh-tests/<label>/runner-with-env.sh web --bind 127.0.0.1:9400
```

Docker images for `local/commit` source:

- By default, images are built locally and SSH targets are updated using `docker save | ssh docker load`
- Use `--push-images` (and optionally `--image-namespace`) to push to GHCR instead
- Use `--skip-docker-images` to skip image builds/updates
