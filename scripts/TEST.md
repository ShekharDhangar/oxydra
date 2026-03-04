#### Automated fresh/upgrade test flows (local + SSH)

If you want repeatable install testing on your Mac and a Raspberry Pi over SSH, use:

```bash
# Fresh isolated install on both hosts (safe: does not touch ~/.local/bin or ~/.oxydra)
./scripts/test-build-install.sh --mode fresh --tag "$OXYDRA_TAG" \
  --target local \
  --target ssh:pi@raspberrypi.local

# Normal upgrade on existing setups
./scripts/test-build-install.sh --mode upgrade --tag "$OXYDRA_TAG" \
  --target local \
  --target ssh:pi@raspberrypi.local

# Discard the fresh isolated install later (use the label printed by fresh mode)
./scripts/test-build-install.sh --mode fresh-clean --label <printed-label> \
  --target local \
  --target ssh:pi@raspberrypi.local
```

`scripts/.env` is auto-loaded when present (gitignored by default). The script writes those values into the fresh install as `runner.env` and generates `start-runner.sh` / `open-web.sh` wrappers so env values override local or remote host environment values during test startup.
Use `--env-file /path/to/file` to use a different local env file, or `--no-env-file` to disable env loading.