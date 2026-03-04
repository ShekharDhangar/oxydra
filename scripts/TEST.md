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

Fresh mode prints commands to launch the onboarding wizard (`runner ... web`) from the isolated install path and a cleanup command to remove it afterwards.
